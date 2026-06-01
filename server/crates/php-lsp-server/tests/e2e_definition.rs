mod support;

use support::*;

#[tokio::test(flavor = "current_thread")]
async fn test_goto_definition() {
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

    let code = r#"<?php
namespace App;

class Foo {
    public function bar(): void {}
}

$f = new Foo();
$f->bar();
"#;
    let uri = "file:///test/Foo.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    // Go to definition on "Foo" in "new Foo()"
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(2, uri, 7, 12))
        .await
        .unwrap();

    let result = extract_result(resp);
    // Should return a location pointing to the class definition
    if !result.is_null() {
        let target_uri = result.get("uri").and_then(|u| u.as_str()).unwrap_or("");
        assert_eq!(target_uri, uri, "definition should point to the same file");
    }

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
async fn test_goto_definition_parent_scope_and_method() {
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

    let base_code = r#"<?php
namespace App;

class Base {
    public function run(): void {}
}
"#;
    let child_code = r#"<?php
namespace App;

class Child extends Base {
    public function test(): void {
        parent::run();
    }
}
"#;
    let base_uri = "file:///test/Base.php";
    let child_uri = "file:///test/ParentDefinition.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(base_uri, base_code))
        .await
        .unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(child_uri, child_code))
        .await
        .unwrap();

    let parent_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(2, child_uri, 5, 8))
        .await
        .unwrap();
    let parent_result = extract_result(parent_resp);

    assert_eq!(
        parent_result.get("uri").and_then(|u| u.as_str()),
        Some(base_uri),
        "definition on parent scope should point to Base class, got: {}",
        parent_result
    );
    assert_eq!(
        parent_result["range"]["start"]["line"].as_u64(),
        Some(3),
        "definition on parent scope should point to Base class line, got: {}",
        parent_result
    );
    assert_eq!(
        parent_result["range"]["start"]["character"].as_u64(),
        Some(6),
        "definition on parent scope should point to Base class name, got: {}",
        parent_result
    );

    let method_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(3, child_uri, 5, 16))
        .await
        .unwrap();
    let method_result = extract_result(method_resp);

    assert_eq!(
        method_result.get("uri").and_then(|u| u.as_str()),
        Some(base_uri),
        "definition on parent method should point to Base::run, got: {}",
        method_result
    );
    assert_eq!(
        method_result["range"]["start"]["line"].as_u64(),
        Some(4),
        "definition on parent method should point to run() line, got: {}",
        method_result
    );
    assert_eq!(
        method_result["range"]["start"]["character"].as_u64(),
        Some(20),
        "definition on parent method should point to run() name, got: {}",
        method_result
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
async fn test_goto_declaration_points_to_import_or_definition_fallback() {
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

class Service {}
"#;
    let vendor_uri = "file:///test/VendorService.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(vendor_uri, vendor_code))
        .await
        .unwrap();

    let app_code = r#"<?php
namespace App;

use Vendor\Service;

class Demo {
    public function run(): void {
        new Service();
    }
}
"#;
    let app_uri = "file:///test/DeclarationImport.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(app_uri, app_code))
        .await
        .unwrap();

    let import_resp = service
        .ready()
        .await
        .unwrap()
        .call(declaration_request(2, app_uri, 7, 12))
        .await
        .unwrap();
    let import_result = extract_result(import_resp);
    assert_eq!(
        import_result.get("uri").and_then(|value| value.as_str()),
        Some(app_uri),
        "declaration for imported class should point to current file use statement, got: {}",
        import_result
    );
    assert_eq!(
        import_result["range"]["start"]["line"].as_u64(),
        Some(3),
        "declaration should start on use statement, got: {}",
        import_result
    );
    assert_eq!(
        import_result["range"]["start"]["character"].as_u64(),
        Some(4),
        "declaration should point to imported FQN inside use statement, got: {}",
        import_result
    );

    let local_code = r#"<?php
namespace App;

class Local {}

new Local();
"#;
    let local_uri = "file:///test/DeclarationLocal.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(local_uri, local_code))
        .await
        .unwrap();

    let fallback_resp = service
        .ready()
        .await
        .unwrap()
        .call(declaration_request(3, local_uri, 5, 5))
        .await
        .unwrap();
    let fallback_result = extract_result(fallback_resp);
    assert_eq!(
        fallback_result.get("uri").and_then(|value| value.as_str()),
        Some(local_uri),
        "declaration without import should fall back to definition, got: {}",
        fallback_result
    );
    assert_eq!(
        fallback_result["range"]["start"]["line"].as_u64(),
        Some(3),
        "fallback declaration should point to class name definition, got: {}",
        fallback_result
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
async fn test_goto_type_definition_for_variables_returns_and_properties() {
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

class Service {}

function makeService(): Service { return new Service(); }

class Demo {
    public Service $service;

    public function run(Service $param): void {
        /** @var Service $local */
        $local = makeService();
        $param;
        $local;
        makeService();
        $this->service;
    }
}
"#;
    let uri = "file:///test/TypeDefinition.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    for (id, line, character, label) in [
        (2, 13, 9, "typed parameter"),
        (3, 14, 9, "inline @var local"),
        (4, 15, 10, "function return"),
        (5, 16, 16, "property type"),
    ] {
        let resp = service
            .ready()
            .await
            .unwrap()
            .call(type_definition_request(id, uri, line, character))
            .await
            .unwrap();
        let result = extract_result(resp);
        assert_eq!(
            result.get("uri").and_then(|value| value.as_str()),
            Some(uri),
            "type definition for {} should point to current file, got: {}",
            label,
            result
        );
        assert_eq!(
            result["range"]["start"]["line"].as_u64(),
            Some(3),
            "type definition for {} should point to Service class, got: {}",
            label,
            result
        );
        assert_eq!(
            result["range"]["start"]["character"].as_u64(),
            Some(6),
            "type definition for {} should point to Service class name, got: {}",
            label,
            result
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
async fn test_goto_implementation_for_types_and_methods() {
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

interface Contract {
    public function work(): void;
}

class Base {
    public function run(): void {}
}

class Impl extends Base implements Contract {
    public function work(): void {}
    public function run(): void {}
}

class Other implements Contract {
    public function work(): void {}
}

function useIt(Contract $contract, Base $base): void {
    $contract->work();
    $base->run();
}
"#;
    let uri = "file:///test/implementation.php";

    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let type_response = service
        .ready()
        .await
        .unwrap()
        .call(implementation_request(2, uri, 3, 12))
        .await
        .unwrap();
    let type_result = extract_result(type_response);
    let type_impls = type_result
        .as_array()
        .expect("expected implementation locations for Contract");
    let type_lines: Vec<u64> = type_impls
        .iter()
        .filter_map(|location| location["range"]["start"]["line"].as_u64())
        .collect();
    assert!(
        type_lines.contains(&11) && type_lines.contains(&16),
        "expected Impl and Other implementation locations, got: {}",
        type_result
    );

    let method_response = service
        .ready()
        .await
        .unwrap()
        .call(implementation_request(3, uri, 4, 22))
        .await
        .unwrap();
    let method_result = extract_result(method_response);
    let method_impls = method_result
        .as_array()
        .expect("expected implementation locations for Contract::work");
    let method_lines: Vec<u64> = method_impls
        .iter()
        .filter_map(|location| location["range"]["start"]["line"].as_u64())
        .collect();
    assert!(
        method_lines.contains(&12) && method_lines.contains(&17),
        "expected Impl::work and Other::work implementation locations, got: {}",
        method_result
    );

    let call_response = service
        .ready()
        .await
        .unwrap()
        .call(implementation_request(4, uri, 21, 17))
        .await
        .unwrap();
    let call_result = extract_result(call_response);
    let call_impls = call_result
        .as_array()
        .expect("expected implementation locations for call-site Contract::work");
    assert_eq!(
        call_impls.len(),
        2,
        "expected two call-site implementations, got: {}",
        call_result
    );

    let override_response = service
        .ready()
        .await
        .unwrap()
        .call(implementation_request(5, uri, 8, 21))
        .await
        .unwrap();
    let override_result = extract_result(override_response);
    let override_impls = override_result
        .as_array()
        .expect("expected implementation locations for Base::run");
    assert!(
        override_impls
            .iter()
            .any(|location| location["range"]["start"]["line"].as_u64() == Some(13)),
        "expected Impl::run override location, got: {}",
        override_result
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
async fn test_goto_definition_foreach_value_variable() {
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
function demo(array $items): void {
    foreach ($items as $item) {
        echo $item;
    }
}
"#;
    let uri = "file:///test/GotoForeachVariable.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(uri, code))
        .await
        .unwrap();

    let (def_line, def_character) = utf16_position_at(code, "$item) {");
    let (usage_line, usage_character) = utf16_position_at(code, "$item;");

    for (id, line, character, label) in [
        (2, usage_line, usage_character + 2, "foreach value usage"),
        (3, def_line, def_character + 2, "foreach value declaration"),
    ] {
        let resp = service
            .ready()
            .await
            .unwrap()
            .call(definition_request(id, uri, line, character))
            .await
            .unwrap();
        let result = extract_result(resp);
        assert_eq!(
            result.get("uri").and_then(|value| value.as_str()),
            Some(uri),
            "definition for {} should point to the current file, got: {}",
            label,
            result
        );
        assert_eq!(
            result["range"]["start"]["line"].as_u64(),
            Some(def_line as u64),
            "definition for {} should point to foreach value line, got: {}",
            label,
            result
        );
        assert_eq!(
            result["range"]["start"]["character"].as_u64(),
            Some(def_character as u64),
            "definition for {} should point at foreach value variable, got: {}",
            label,
            result
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn test_goto_definition_variables_and_constants() {
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

    let var_code = r#"<?php
function demo(): void {
    $value = 1;
    echo $value;
}
"#;
    let var_uri = "file:///test/GotoVariable.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(var_uri, var_code))
        .await
        .unwrap();

    // 1) Variable usage -> assignment definition
    let var_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(2, var_uri, 3, 10))
        .await
        .unwrap();
    let var_result = extract_result(var_resp);
    let var_def_line = var_result
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|n| n.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(
        var_def_line, 2,
        "variable usage should go to assignment line"
    );

    // 2) preg_match output variable -> completion and definition at output argument
    let preg_code_with_marker = r#"<?php
function demo(string $value): void {
    if (!preg_match('/(?P<year>\d+)/', $value, $matches)) {
        return;
    }
    $mat/*caret*/;
    echo $matches['year'];
}
"#;
    let marker = "/*caret*/";
    let marker_offset = preg_code_with_marker
        .find(marker)
        .expect("test code should contain caret marker");
    let preg_code = preg_code_with_marker.replace(marker, "");
    let marker_prefix = &preg_code[..marker_offset];
    let completion_line = marker_prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let completion_line_start = marker_prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let completion_character = (marker_prefix.len() - completion_line_start) as u32;
    let preg_uri = "file:///test/GotoPregMatchOutput.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(preg_uri, &preg_code))
        .await
        .unwrap();

    let completion_resp = service
        .ready()
        .await
        .unwrap()
        .call(completion_request(
            3,
            preg_uri,
            completion_line,
            completion_character,
        ))
        .await
        .unwrap();
    let completion_result = extract_result(completion_resp);
    let completion_labels: Vec<String> = completion_items_from_result(&completion_result)
        .iter()
        .filter_map(|item| item.get("label").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    assert!(
        completion_labels.iter().any(|label| label == "$matches"),
        "variable completion should include preg_match output variable, got: {:?}",
        completion_labels
    );

    let output_offset = preg_code
        .find("$matches))")
        .expect("test code should contain preg_match output variable");
    let usage_offset = preg_code
        .find("$matches['year']")
        .expect("test code should contain preg_match output variable usage")
        + 2;
    let usage_prefix = &preg_code[..usage_offset];
    let usage_line = usage_prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let usage_line_start = usage_prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let usage_character = (usage_prefix.len() - usage_line_start) as u32;
    let output_prefix = &preg_code[..output_offset];
    let output_line = output_prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let output_line_start = output_prefix.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
    let output_character = (output_prefix.len() - output_line_start) as u64;

    let preg_def_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(4, preg_uri, usage_line, usage_character))
        .await
        .unwrap();
    let preg_def_result = extract_result(preg_def_resp);
    assert_eq!(
        preg_def_result["range"]["start"]["line"].as_u64(),
        Some(output_line as u64),
        "preg_match output variable usage should go to output argument, got: {}",
        preg_def_result
    );
    assert_eq!(
        preg_def_result["range"]["start"]["character"].as_u64(),
        Some(output_character),
        "preg_match output variable usage should point at output argument variable, got: {}",
        preg_def_result
    );

    // 3) $this usage -> containing class declaration
    let this_code = r#"<?php
namespace App;

class Demo {
    public function run(): void {
        $this;
        $this->run();
    }
}
"#;
    let this_uri = "file:///test/GotoThis.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(this_uri, this_code))
        .await
        .unwrap();

    for (id, line, character, label) in [
        (5, 5, 10, "standalone $this"),
        (6, 6, 10, "$this in member access"),
    ] {
        let this_resp = service
            .ready()
            .await
            .unwrap()
            .call(definition_request(id, this_uri, line, character))
            .await
            .unwrap();
        let this_result = extract_result(this_resp);
        assert_eq!(
            this_result.get("uri").and_then(|value| value.as_str()),
            Some(this_uri),
            "definition for {} should point to the current class, got: {}",
            label,
            this_result
        );
        assert_eq!(
            this_result["range"]["start"]["line"].as_u64(),
            Some(3),
            "definition for {} should point to class name line, got: {}",
            label,
            this_result
        );
        assert_eq!(
            this_result["range"]["start"]["character"].as_u64(),
            Some(6),
            "definition for {} should point to class name character, got: {}",
            label,
            this_result
        );
    }

    // 4) Global const usage -> const declaration
    let const_code = r#"<?php
namespace App;

const BUILD = 'dev';

echo BUILD;
"#;
    let const_uri = "file:///test/GotoConstant.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(const_uri, const_code))
        .await
        .unwrap();

    let build_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(7, const_uri, 5, 5))
        .await
        .unwrap();
    let build_result = extract_result(build_resp);
    let build_def_line = build_result
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|n| n.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(
        build_def_line, 3,
        "constant usage should go to const declaration line"
    );

    // 5) self::CLASS_CONST usage -> class const declaration
    let class_const_code = r#"<?php
namespace App;

class Foo {
    public const VERSION = '1.0';
    public function run(): string {
        return self::VERSION;
    }
}
"#;
    let class_const_uri = "file:///test/GotoClassConstant.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(class_const_uri, class_const_code))
        .await
        .unwrap();

    let cc_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(8, class_const_uri, 6, 21))
        .await
        .unwrap();
    let cc_result = extract_result(cc_resp);
    let cc_def_line = cc_result
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|n| n.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(
        cc_def_line, 4,
        "class constant usage should go to class const declaration line"
    );

    // 6) Static property usages: self::$created, static::$var, User::$var
    let static_prop_code = r#"<?php
namespace App;

class User {
    public static string $var = 'u';
}

class Demo {
    public static string $created = 'c';
    public static string $var = 'd';

    public function run(): void {
        echo self::$created;
        echo static::$var;
        echo User::$var;
    }
}
"#;
    let static_prop_uri = "file:///test/GotoStaticProperty.php";
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(static_prop_uri, static_prop_code))
        .await
        .unwrap();

    let self_prop_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(9, static_prop_uri, 12, 21))
        .await
        .unwrap();
    let self_prop_result = extract_result(self_prop_resp);
    let self_prop_line = self_prop_result
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|n| n.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(
        self_prop_line, 8,
        "self::$created should go to static property declaration"
    );

    let static_prop_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(10, static_prop_uri, 13, 23))
        .await
        .unwrap();
    let static_prop_result = extract_result(static_prop_resp);
    let static_prop_line = static_prop_result
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|n| n.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(
        static_prop_line, 9,
        "static::$var should go to class static property declaration"
    );

    let user_prop_resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(11, static_prop_uri, 14, 20))
        .await
        .unwrap();
    let user_prop_result = extract_result(user_prop_resp);
    let user_prop_line = user_prop_result
        .get("range")
        .and_then(|r| r.get("start"))
        .and_then(|s| s.get("line"))
        .and_then(|n| n.as_u64())
        .unwrap_or(u64::MAX);
    assert_eq!(
        user_prop_line, 4,
        "User::$var should go to referenced class static property declaration"
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
async fn test_goto_definition_vendor_inherited_method() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let fixture_root = support::vendor_resolve_fixture_root();
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
    tokio::task::yield_now().await;

    // Open SampleTest.php
    let test_path = fixture_root.join("tests/SampleTest.php");
    let test_file_uri = format!("file://{}", test_path.display());
    let content = fs::read_to_string(&test_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&test_file_uri, &content))
        .await
        .unwrap();

    // Cursor on "createStub" in:  $stub = $this->createStub(TimerService::class);
    // Line 40, col 23 (0-indexed)
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(10, &test_file_uri, 40, 23))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(
        !result.is_null(),
        "go-to-definition on createStub() should resolve to vendor BaseAssert::createStub"
    );

    let target_uri = result.get("uri").and_then(|u| u.as_str()).unwrap_or("");
    assert!(
        target_uri.contains("BaseAssert.php"),
        "definition should point to BaseAssert.php, got: {}",
        target_uri
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

/// Bug 1: go-to-definition on a vendor method via typed property in same file.
///
/// `$this->timerMock->method('start')` in SampleTest where:
///   timerMock is `private MockBuilder $timerMock` (same file),
///   MockBuilder is a vendor class with method().
///
/// Requires: stripping `::member` from FQN for vendor PSR-4 lookup.

#[tokio::test(flavor = "current_thread")]
async fn test_goto_definition_vendor_method_via_typed_property() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let fixture_root = support::vendor_resolve_fixture_root();
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
    tokio::task::yield_now().await;

    // Open SampleTest.php
    let test_path = fixture_root.join("tests/SampleTest.php");
    let test_file_uri = format!("file://{}", test_path.display());
    let content = fs::read_to_string(&test_path).unwrap();
    service
        .ready()
        .await
        .unwrap()
        .call(did_open_notification(&test_file_uri, &content))
        .await
        .unwrap();

    // Cursor on "method" in:  $this->timerMock->method('start');
    // Line 46, col 26 (0-indexed)
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(10, &test_file_uri, 46, 26))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(
        !result.is_null(),
        "go-to-definition on method() should resolve to vendor MockBuilder::method"
    );

    let target_uri = result.get("uri").and_then(|u| u.as_str()).unwrap_or("");
    assert!(
        target_uri.contains("MockBuilder.php"),
        "definition should point to MockBuilder.php, got: {}",
        target_uri
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

/// Bug 3 (cross-file): go-to-definition on method of a property declared in parent class.
///
/// `$this->timer->start('handle')` in ConcreteHandler where:
///   ConcreteHandler extends BaseHandler,
///   BaseHandler declares `protected TimerService $timer`.
///   The property is NOT in ConcreteHandler's file_symbols.
///
/// Requires: cross-file property type resolution (callback into WorkspaceIndex).

#[tokio::test(flavor = "current_thread")]
async fn test_goto_definition_cross_file_inherited_property_method() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let fixture_root = support::vendor_resolve_fixture_root();
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
    tokio::task::yield_now().await;

    // Open all needed files
    for rel in &[
        "src/ConcreteHandler.php",
        "src/BaseHandler.php",
        "src/TimerService.php",
    ] {
        let p = fixture_root.join(rel);
        let u = format!("file://{}", p.display());
        let c = fs::read_to_string(&p).unwrap();
        service
            .ready()
            .await
            .unwrap()
            .call(did_open_notification(&u, &c))
            .await
            .unwrap();
    }

    let handler_uri = format!("file://{}/src/ConcreteHandler.php", fixture_root.display());

    // Cursor on "start" in:  $this->timer->start('handle');
    // Line 21, col 22 (0-indexed)
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(10, &handler_uri, 21, 22))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(
        !result.is_null(),
        "go-to-definition on start() via inherited property $timer should resolve to TimerService::start"
    );

    let target_uri = result.get("uri").and_then(|u| u.as_str()).unwrap_or("");
    assert!(
        target_uri.contains("TimerService.php"),
        "definition should point to TimerService.php, got: {}",
        target_uri
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}

/// Same-project cross-file: go-to-definition on method via property in same file.
///
/// `$this->timerService->start('benchmark')` in SampleTest where:
///   timerService is `private TimerService $timerService` (same file),
///   TimerService is a local class opened via did_open.
///
/// This validates the basic chained access + cross-file method resolution.

#[tokio::test(flavor = "current_thread")]
async fn test_goto_definition_cross_file_method_via_same_file_property() {
    let (mut service, socket) = LspService::new(PhpLspBackend::new);
    tokio::spawn(async move {
        socket.collect::<Vec<_>>().await;
    });

    let fixture_root = support::vendor_resolve_fixture_root();
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
    tokio::task::yield_now().await;

    // Open both needed files
    for rel in &["tests/SampleTest.php", "src/TimerService.php"] {
        let p = fixture_root.join(rel);
        let u = format!("file://{}", p.display());
        let c = fs::read_to_string(&p).unwrap();
        service
            .ready()
            .await
            .unwrap()
            .call(did_open_notification(&u, &c))
            .await
            .unwrap();
    }

    let test_file_uri = format!("file://{}/tests/SampleTest.php", fixture_root.display());

    // Cursor on "start" in:  $this->timerService->start('benchmark');
    // Line 58, col 29 (0-indexed)
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(definition_request(10, &test_file_uri, 58, 29))
        .await
        .unwrap();

    let result = extract_result(resp);
    assert!(
        !result.is_null(),
        "go-to-definition on start() via $timerService property should resolve to TimerService::start"
    );

    let target_uri = result.get("uri").and_then(|u| u.as_str()).unwrap_or("");
    assert!(
        target_uri.contains("TimerService.php"),
        "definition should point to TimerService.php, got: {}",
        target_uri
    );

    service
        .ready()
        .await
        .unwrap()
        .call(shutdown_request(99))
        .await
        .unwrap();
}
