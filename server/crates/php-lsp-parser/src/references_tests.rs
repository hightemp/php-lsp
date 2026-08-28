use super::*;
use crate::parser::FileParser;
use crate::symbols::extract_file_symbols;

fn find_refs(code: &str, target_fqn: &str, kind: PhpSymbolKind) -> Vec<ReferenceLocation> {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    find_references_in_file(tree, code, &file_symbols, target_fqn, kind, true)
}

fn collect_refs(code: &str) -> Vec<SymbolReference> {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    collect_symbol_references_in_file(tree, code, &file_symbols)
}

fn synthetic_symbol_reference(
    target_kind: PhpSymbolKind,
    starts_with_dollar: bool,
) -> SymbolReference {
    SymbolReference {
        target_fqn: "App\\Target::member".to_string(),
        target_kind,
        range: (4, 8, 4, 14),
        is_declaration: false,
        starts_with_dollar,
        allows_global_fallback: false,
        rename_range: None,
        preserve_spelling_on_rename: false,
        is_import_target: false,
        receiver: SymbolReferenceReceiver::ResolvedType {
            type_fqn: "App\\Target".to_string(),
        },
    }
}

fn find_var_refs_at(
    code: &str,
    line: u32,
    col: u32,
    include_declaration: bool,
) -> Vec<ReferenceLocation> {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    find_variable_references_at_position(tree, code, line, col, include_declaration)
}

fn find_line_col(code: &str, needle: &str) -> (u32, u32) {
    for (line, row) in code.lines().enumerate() {
        if let Some(col) = row.find(needle) {
            return (line as u32, col as u32);
        }
    }
    panic!("needle not found: {}", needle);
}

#[test]
fn test_find_class_references_new() {
    let code = r#"<?php
namespace App;

use App\Service\UserService;

$svc = new UserService();
$svc2 = new UserService();
"#;
    let refs = find_refs(code, "App\\Service\\UserService", PhpSymbolKind::Class);
    assert_eq!(refs.len(), 2, "Should find 2 new-expression references");
}

#[test]
fn test_collected_reference_ranges_are_utf16_after_emoji() {
    let code = "<?php\nnamespace App;\nclass Foo {}\n$emoji = \"😀\"; new Foo();\n";
    let refs = collect_refs(code);

    assert!(
        refs.iter().any(|reference| {
            reference.target_fqn == "App\\Foo"
                && !reference.is_declaration
                && reference.range == (3, 19, 3, 22)
        }),
        "class reference after emoji should use UTF-16 range, got {refs:?}"
    );
}

#[test]
fn test_variable_reference_ranges_are_utf16_after_emoji() {
    let code = "<?php\n$emoji = \"😀\"; $target = 1; echo $target;\n";
    let target_byte_col = code.lines().nth(1).unwrap().find("$target").unwrap() as u32;
    let refs = find_var_refs_at(code, 1, target_byte_col, true);

    assert!(
        refs.iter()
            .any(|reference| reference.range == (1, 17, 1, 24)),
        "declaration range should use UTF-16 columns, got {refs:?}"
    );
    assert!(
        refs.iter()
            .any(|reference| reference.range == (1, 35, 1, 42)),
        "usage range should use UTF-16 columns, got {refs:?}"
    );
}

#[test]
fn test_find_class_references_type_hint() {
    let code = r#"<?php
namespace App;

use App\Model\User;

class Controller {
    public function show(User $user): User {
        return $user;
    }
}
"#;
    let refs = find_refs(code, "App\\Model\\User", PhpSymbolKind::Class);
    // Should find type hint in param + return type = 2
    assert!(
        refs.len() >= 2,
        "Should find at least 2 type hint references, found {}",
        refs.len()
    );
}

#[test]
fn test_find_function_references() {
    let code = r#"<?php
namespace App;

function helper() {}

helper();
helper();
"#;
    let refs = find_refs(code, "App\\helper", PhpSymbolKind::Function);
    // 1 declaration + 2 calls = 3
    assert_eq!(refs.len(), 3, "Should find declaration + 2 calls");
}

#[test]
fn test_find_class_and_function_references_case_insensitively() {
    let code = r#"<?php
namespace App;
class MixedCaseClass {}
function MixedCaseFunction() {}
new mixedcaseclass();
MIXEDCASEFUNCTION();
"#;

    let class_refs = find_refs(code, r"app\MIXEDCASECLASS", PhpSymbolKind::Class);
    assert_eq!(class_refs.len(), 2);

    let function_refs = find_refs(code, r"APP\mixedcasefunction", PhpSymbolKind::Function);
    assert_eq!(function_refs.len(), 2);
}

#[test]
fn test_collect_references_uses_each_namespace_block_import_scope() {
    let code = r#"<?php
namespace First {
    use Vendor\First\Service as Shared;
    new Shared();
}
namespace Second {
    use Vendor\Second\Service as Shared;
    new Shared();
}
"#;
    let refs = collect_refs(code);

    assert!(refs.iter().any(|reference| {
        reference.target_fqn == r"Vendor\First\Service" && reference.range.0 == 3
    }));
    assert!(refs.iter().any(|reference| {
        reference.target_fqn == r"Vendor\Second\Service" && reference.range.0 == 7
    }));
}

#[test]
fn test_collect_references_resolves_namespace_relative_and_qualified_functions() {
    let code = r#"<?php
namespace App\Feature;
namespace\helper();
Support\other();
"#;
    let refs = collect_refs(code);

    assert!(refs.iter().any(|reference| {
        reference.target_kind == PhpSymbolKind::Function
            && reference.target_fqn == r"App\Feature\helper"
            && !reference.allows_global_fallback
    }));
    assert!(refs.iter().any(|reference| {
        reference.target_kind == PhpSymbolKind::Function
            && reference.target_fqn == r"App\Feature\Support\other"
            && !reference.allows_global_fallback
    }));
}

#[test]
fn test_global_function_reference_fallback_requires_unqualified_source_name() {
    let code = r#"<?php
namespace App;
strlen('plain');
Support\strlen('qualified');
"#;

    let global_refs = find_refs(code, "strlen", PhpSymbolKind::Function);
    assert_eq!(
        global_refs
            .iter()
            .map(|reference| reference.range.0)
            .collect::<Vec<_>>(),
        vec![2],
        "only the unqualified call may match the canonical global function"
    );

    let collected = collect_refs(code);
    let plain = collected
        .iter()
        .find(|reference| {
            reference.target_kind == PhpSymbolKind::Function && reference.range.0 == 2
        })
        .expect("unqualified function reference should be collected");
    assert_eq!(plain.target_fqn, r"App\strlen");
    assert!(plain.allows_global_fallback);

    let qualified = collected
        .iter()
        .find(|reference| {
            reference.target_kind == PhpSymbolKind::Function && reference.range.0 == 3
        })
        .expect("qualified function reference should be collected");
    assert!(!qualified.allows_global_fallback);

    let explicit = collect_refs(
        r#"<?php
namespace App\Feature;
namespace\strlen('explicit');
Support\other();
"#,
    );
    let explicit = explicit
        .iter()
        .find(|reference| reference.target_kind == PhpSymbolKind::Function)
        .expect("namespace-relative function reference should be collected");
    assert!(!explicit.allows_global_fallback);
}

#[test]
fn test_collect_references_expands_namespace_aliases_for_qualified_symbols() {
    let code = r#"<?php
namespace App;
use Vendor\Package as Lib;
use const Vendor\FLAG as ConstAlias;
lib\helper();
echo LIB\FLAG;
echo ConstAlias\CHILD;
"#;
    let refs = collect_refs(code);

    assert!(refs.iter().any(|reference| {
        reference.target_kind == PhpSymbolKind::Function
            && reference.target_fqn == r"Vendor\Package\helper"
    }));
    assert!(refs.iter().any(|reference| {
        reference.target_kind == PhpSymbolKind::GlobalConstant
            && reference.target_fqn == r"Vendor\Package\FLAG"
    }));
    assert!(refs.iter().any(|reference| {
        reference.target_kind == PhpSymbolKind::GlobalConstant
            && reference.target_fqn == r"App\ConstAlias\CHILD"
    }));
    assert!(!refs.iter().any(|reference| {
        reference.target_kind == PhpSymbolKind::GlobalConstant
            && reference.target_fqn == r"Vendor\FLAG\CHILD"
    }));
}

#[test]
fn test_global_constant_references_ignore_namespace_case_only() {
    let code = r#"<?php
echo \Vendor\Package\FLAG;
"#;

    assert!(!find_refs(code, r"vendor\package\FLAG", PhpSymbolKind::GlobalConstant).is_empty());
    assert!(find_refs(code, r"vendor\package\flag", PhpSymbolKind::GlobalConstant).is_empty());
}

#[test]
fn test_find_static_method_references() {
    let code = r#"<?php
namespace App;

class Foo {
    public static function bar() {}
}

Foo::bar();
"#;
    let refs = find_refs(code, "App\\Foo::bar", PhpSymbolKind::Method);
    // declaration + 1 call = 2
    assert!(!refs.is_empty(), "Should find at least 1 reference");
}

#[test]
fn test_find_method_references_case_insensitively() {
    let code = r#"<?php
namespace App;

class Foo {
    public static function propFind() {}

    public function run(): void {
        self::propfind();
        $this->PROPFIND();
        $this->propFind;
    }
}

Foo::PROPFIND();
"#;
    let refs = find_refs(code, "App\\Foo::propFind", PhpSymbolKind::Method);

    assert_eq!(
            refs.len(),
            4,
            "Method references should include declaration and differently-cased calls, but not property access"
        );
}

#[test]
fn test_find_property_references_not_method_with_same_name() {
    let code = r#"<?php
namespace App;

class Baz {
    public string $test = '';
    public function test(): string { return 'ok'; }
}

function run(Baz $baz): void {
    echo $baz->test;
    $baz->test();
}
"#;

    let refs = find_refs(code, "App\\Baz::$test", PhpSymbolKind::Property);
    // declaration + one property usage
    assert_eq!(
        refs.len(),
        2,
        "Property references should not include method calls with the same name"
    );
}

#[test]
fn test_find_class_constant_references() {
    let code = r#"<?php
namespace App;

class RenameTarget {
    public const STATE_ACTIVE = 'active';
    public function touch(): void {
        echo self::STATE_ACTIVE;
    }
}

echo RenameTarget::STATE_ACTIVE;
"#;
    let refs = find_refs(
        code,
        "App\\RenameTarget::STATE_ACTIVE",
        PhpSymbolKind::ClassConstant,
    );
    // declaration + 2 usages
    assert_eq!(refs.len(), 3, "Should find declaration + 2 constant usages");
}

#[test]
fn test_symbol_reference_sort_key_matches_dedup_key() {
    let duplicate_method = synthetic_symbol_reference(PhpSymbolKind::Method, false);
    let distinct_property_same_range = synthetic_symbol_reference(PhpSymbolKind::Property, false);
    let distinct_method_with_dollar = synthetic_symbol_reference(PhpSymbolKind::Method, true);
    let mut refs = vec![
        duplicate_method.clone(),
        distinct_property_same_range,
        duplicate_method,
        distinct_method_with_dollar,
    ];

    sort_and_dedup_symbol_references(&mut refs);

    assert_eq!(
        refs.len(),
        3,
        "same-kind duplicates should collapse, but different kind/dollar state must survive"
    );
    assert_eq!(
        refs.iter()
            .filter(|reference| reference.target_kind == PhpSymbolKind::Method
                && !reference.starts_with_dollar)
            .count(),
        1
    );
    assert!(refs.iter().any(|reference| {
        reference.target_kind == PhpSymbolKind::Property && !reference.starts_with_dollar
    }));
    assert!(refs.iter().any(|reference| {
        reference.target_kind == PhpSymbolKind::Method && reference.starts_with_dollar
    }));
}

#[test]
fn test_collect_symbol_references_resolves_imported_global_constants() {
    let code = r#"<?php
namespace App;

use const Vendor\FLAGS\ENABLED as IS_ENABLED;

echo IS_ENABLED;
"#;
    let refs = collect_refs(code);

    assert!(refs.iter().any(|reference| {
        reference.target_fqn == "Vendor\\FLAGS\\ENABLED"
            && reference.target_kind == PhpSymbolKind::GlobalConstant
            && !reference.is_declaration
    }));
}

#[test]
fn test_import_references_preserve_explicit_aliases_for_rename() {
    let code = r#"<?php
namespace App;

use Vendor\{
    Service as Alias,
    ImplicitService,
    function helper as call_it,
    function plain_helper,
    const FLAG as LOCAL_FLAG,
    const OTHER
};

new Alias();
new ImplicitService();
call_it();
plain_helper();
echo LOCAL_FLAG;
echo OTHER;
"#;
    let refs = collect_refs(code);

    for (target_fqn, target_kind) in [
        (r"Vendor\Service", PhpSymbolKind::Class),
        (r"Vendor\helper", PhpSymbolKind::Function),
        (r"Vendor\FLAG", PhpSymbolKind::GlobalConstant),
    ] {
        let matching: Vec<_> = refs
            .iter()
            .filter(|reference| {
                reference.target_fqn == target_fqn && reference.target_kind == target_kind
            })
            .collect();
        assert_eq!(
            matching.len(),
            2,
            "expected import target plus aliased usage for {target_fqn}: {matching:?}"
        );
        assert_eq!(
            matching
                .iter()
                .filter(|reference| reference.preserve_spelling_on_rename)
                .count(),
            1,
            "only the explicit alias usage should preserve spelling: {matching:?}"
        );
        assert_eq!(
            matching
                .iter()
                .filter(|reference| reference.is_import_target)
                .count(),
            1,
            "one reference must identify the import target: {matching:?}"
        );
        assert!(matching
            .iter()
            .all(|reference| reference.rename_range.is_some()));
    }

    for (target_fqn, target_kind) in [
        (r"Vendor\ImplicitService", PhpSymbolKind::Class),
        (r"Vendor\plain_helper", PhpSymbolKind::Function),
        (r"Vendor\OTHER", PhpSymbolKind::GlobalConstant),
    ] {
        let matching: Vec<_> = refs
            .iter()
            .filter(|reference| {
                reference.target_fqn == target_fqn && reference.target_kind == target_kind
            })
            .collect();
        assert_eq!(
            matching.len(),
            2,
            "expected import target plus implicit usage for {target_fqn}: {matching:?}"
        );
        assert!(
            matching
                .iter()
                .all(|reference| !reference.preserve_spelling_on_rename),
            "implicit aliases must be renamed with their target: {matching:?}"
        );
        assert_eq!(
            matching
                .iter()
                .filter(|reference| reference.is_import_target)
                .count(),
            1,
            "one reference must identify the import target: {matching:?}"
        );
        assert!(matching
            .iter()
            .all(|reference| reference.rename_range.is_some()));
    }
}

#[test]
fn test_collect_symbol_references_for_workspace_index() {
    let code = r#"<?php
namespace App;

use App\Model\User;

const FLAG = true;
function helper(): void {}

class Service {
    public const STATE = 'ok';
    public string $name = '';

    public function run(User $user): void {
        helper();
        echo self::STATE;
        echo $this->name;
        echo FLAG;
    }
}

$service = new Service();
"#;

    let refs = collect_refs(code);

    assert!(refs.iter().any(|reference| {
        reference.target_fqn == "App\\Service"
            && reference.target_kind == PhpSymbolKind::Class
            && reference.is_declaration
    }));
    assert!(refs.iter().any(|reference| {
        reference.target_fqn == "App\\Service"
            && reference.target_kind == PhpSymbolKind::Class
            && !reference.is_declaration
    }));
    assert!(refs.iter().any(|reference| {
        reference.target_fqn == "App\\Model\\User"
            && reference.target_kind == PhpSymbolKind::Class
            && !reference.is_declaration
    }));
    assert!(refs.iter().any(|reference| {
        reference.target_fqn == "App\\helper"
            && reference.target_kind == PhpSymbolKind::Function
            && !reference.is_declaration
    }));
    assert!(refs.iter().any(|reference| {
        reference.target_fqn == "App\\Service::STATE"
            && reference.target_kind == PhpSymbolKind::ClassConstant
            && !reference.is_declaration
    }));
    assert!(refs.iter().any(|reference| {
        reference.target_fqn == "App\\Service::$name"
            && reference.target_kind == PhpSymbolKind::Property
            && !reference.is_declaration
            && !reference.starts_with_dollar
            && reference.receiver
                == SymbolReferenceReceiver::ResolvedType {
                    type_fqn: "App\\Service".to_string(),
                }
    }));
    assert!(refs.iter().any(|reference| {
        reference.target_fqn == "App\\FLAG"
            && reference.target_kind == PhpSymbolKind::GlobalConstant
            && !reference.is_declaration
    }));
}

#[test]
fn test_find_variable_references_in_function_scope() {
    let code = r#"<?php
function run(string $x): void {
    $x = $x . "!";
    echo $x;
}
"#;
    let (line, col) = find_line_col(code, "echo $x;");
    let refs = find_var_refs_at(code, line, col + 6, true);
    // param + assignment left + assignment right + echo usage
    assert_eq!(refs.len(), 4);

    let refs_no_decl = find_var_refs_at(code, line, col + 6, false);
    // assignment right + echo usage
    assert_eq!(refs_no_decl.len(), 2);
}

#[test]
fn test_find_variable_references_marks_foreach_wrapped_values_as_declarations() {
    let code = r#"<?php
function run(array $items): void {
    foreach ($items as &$value) {
        echo $value;
    }
}
"#;
    let (line, col) = find_line_col(code, "echo $value;");
    let refs = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, true);
    assert_eq!(
        refs.len(),
        2,
        "by-reference foreach value should count as declaration + body usage"
    );

    let refs_no_decl = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, false);
    assert_eq!(
        refs_no_decl.len(),
        1,
        "by-reference foreach declaration should not be counted as a read usage"
    );
}

#[test]
fn test_find_variable_references_marks_foreach_key_and_value_as_declarations() {
    let code = r#"<?php
function run(array $items): void {
    foreach ($items as $key => $value) {
        echo $key;
        echo $value;
    }
}
"#;
    for (needle, var_name) in [("echo $key;", "$key"), ("echo $value;", "$value")] {
        let (line, col) = find_line_col(code, needle);
        let refs = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, true);
        assert_eq!(
            refs.len(),
            2,
            "foreach {var_name} should count as declaration + body usage"
        );

        let refs_no_decl = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, false);
        assert_eq!(
            refs_no_decl.len(),
            1,
            "foreach {var_name} declaration should be excluded when requested"
        );
    }
}

#[test]
fn test_find_variable_references_marks_foreach_destructuring_values_as_declarations() {
    let code = r#"<?php
function run(array $rows): void {
    foreach ($rows as ['id' => $id, 'name' => $name]) {
        echo $id;
        echo $name;
    }
}
"#;
    for (needle, var_name) in [("echo $id;", "$id"), ("echo $name;", "$name")] {
        let (line, col) = find_line_col(code, needle);
        let refs = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, true);
        assert_eq!(
            refs.len(),
            2,
            "foreach destructured {var_name} should count as declaration + body usage"
        );

        let refs_no_decl = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, false);
        assert_eq!(
            refs_no_decl.len(),
            1,
            "foreach destructured {var_name} declaration should be excluded when requested"
        );
    }
}

#[test]
fn test_foreach_binding_roles_use_cst_structure_with_delimiters_in_iterable() {
    let cases = [
        r#"<?php
function run(): void {
    foreach (['{'] as $value) {
        echo $value;
    }
}
"#,
        r#"<?php
function run(): void {
    foreach ([':' => 1] as $value):
        echo $value;
    endforeach;
}
"#,
        r#"<?php
function run(): void {
    foreach ([function (): void {}] as $value) {
        echo $value;
    }
}
"#,
        r#"<?php
function run(array $items): void {
    foreach (/* before iterable */ $items as $value) {
        echo $value;
    }
}
"#,
        r#"<?php
function run(array $items): void {
    foreach ($items /* between iterable and as */ as $value) {
        echo $value;
    }
}
"#,
    ];

    for code in cases {
        let (line, col) = find_line_col(code, "echo $value;");
        let all = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, true);
        assert_eq!(all.len(), 2, "declaration plus read for:\n{code}");

        let reads = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, false);
        assert_eq!(
            reads.len(),
            1,
            "foreach CST target must remain a declaration for:\n{code}"
        );
    }
}

#[test]
fn test_find_variable_references_do_not_cross_scope() {
    let code = r#"<?php
$x = 1;
function demo(): void {
    $x = 2;
    echo $x;
}
echo $x;
"#;
    let (line, col) = find_line_col(code, "echo $x;");
    let refs = find_var_refs_at(code, line, col + 6, true);
    // only inner assignment + inner usage
    assert_eq!(refs.len(), 2);
}

#[test]
fn test_outer_variable_follows_only_real_nested_captures() {
    let code = r#"<?php
function outer(string $value): void {
    echo $value;
    function nested(string $value): void {
        echo $value;
    }
    $plain = function (): void {
        $value = 'local';
        echo $value;
    };
    $captured = function () use (&$value): void {
        echo $value;
        $nestedCapture = fn (): string => $value;
    };
    $shadow = fn (string $value): string => $value;
    echo $value;
}
"#;
    let (line, col) = find_line_col(code, "echo $value;");
    let refs = find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, true);
    let lines: Vec<u32> = refs.iter().map(|reference| reference.range.0).collect();
    assert_eq!(lines, vec![1, 2, 10, 11, 12, 15]);

    let refs_without_declarations =
        find_var_refs_at(code, line, col + "echo ".len() as u32 + 1, false);
    let lines: Vec<u32> = refs_without_declarations
        .iter()
        .map(|reference| reference.range.0)
        .collect();
    assert_eq!(
        lines,
        vec![2, 10, 11, 12, 15],
        "closure use token is a read of the outer binding"
    );
}

#[test]
fn test_top_level_variable_does_not_enter_named_function_scope() {
    let code = r#"<?php
$value = 'global';
function nested(string $value): void {
    echo $value;
}
echo $value;
"#;
    let (line, col) = find_line_col(code, "$value = 'global'");
    let refs = find_var_refs_at(code, line, col + 1, true);
    let lines: Vec<u32> = refs.iter().map(|reference| reference.range.0).collect();
    assert_eq!(lines, vec![1, 5]);
}

#[test]
fn test_global_declaration_does_not_enable_cross_callable_local_rename() {
    let code = r#"<?php
$value = 'global';
function nested(): void {
    global $value;
    echo $value;
}
echo $value;
"#;
    let (outer_line, outer_col) = find_line_col(code, "$value = 'global'");
    let outer_refs = find_var_refs_at(code, outer_line, outer_col + 1, true);
    let lines: Vec<u32> = outer_refs
        .iter()
        .map(|reference| reference.range.0)
        .collect();
    assert_eq!(lines, vec![1, 6]);

    let (inner_line, inner_col) = find_line_col(code, "global $value");
    let inner_refs = find_var_refs_at(
        code,
        inner_line,
        inner_col + "global ".len() as u32 + 1,
        true,
    );
    let lines: Vec<u32> = inner_refs
        .iter()
        .map(|reference| reference.range.0)
        .collect();
    assert!(
        lines.is_empty(),
        "function-local global alias cannot be renamed safely without global symbol expansion"
    );
}

#[test]
fn test_top_level_global_is_a_noop_for_local_binding_collection() {
    let code = r#"<?php
$value = 1;
global $value;
echo $value;
"#;
    let (line, col) = find_line_col(code, "$value = 1");
    let refs = find_var_refs_at(code, line, col + 1, true);
    let lines: Vec<u32> = refs.iter().map(|reference| reference.range.0).collect();
    assert_eq!(lines, vec![1, 2, 3]);
}

#[test]
fn test_global_in_capture_connected_callable_fails_closed() {
    let code = r#"<?php
function outer(string $value): void {
    $closure = function () use ($value): void {
        if (false) {
            global $value;
        }
        echo $value;
    };
    echo $value;
}
"#;
    let (outer_line, outer_col) = find_line_col(code, "string $value");
    let outer_refs = find_var_refs_at(
        code,
        outer_line,
        outer_col + "string ".len() as u32 + 1,
        true,
    );
    assert!(
        outer_refs.is_empty(),
        "a runtime-conditional global makes the connected binding unsafe to rename"
    );

    let (global_line, global_col) = find_line_col(code, "global $value");
    let global_refs = find_var_refs_at(
        code,
        global_line,
        global_col + "global ".len() as u32 + 1,
        true,
    );
    assert!(global_refs.is_empty());

    let (body_line, body_col) = find_line_col(code, "        echo $value;");
    let body_refs = find_var_refs_at(
        code,
        body_line,
        body_col + "        echo ".len() as u32 + 1,
        true,
    );
    assert!(body_refs.is_empty());
}

#[test]
fn test_cursor_inside_captured_closure_reaches_outer_binding() {
    let code = r#"<?php
function outer(string $value): void {
    echo $value;
    $captured = function () use ($value): void {
        echo $value;
        $nested = fn (): string => $value;
    };
    echo $value;
}
"#;
    let (line, col) = find_line_col(code, "$nested =");
    let refs = find_var_refs_at(
        code,
        line,
        col + "$nested = fn (): string => ".len() as u32 + 1,
        true,
    );
    let lines: Vec<u32> = refs.iter().map(|reference| reference.range.0).collect();
    assert_eq!(lines, vec![1, 2, 3, 4, 5, 7]);
}

#[test]
fn test_uncaptured_closure_variable_stays_local() {
    let code = r#"<?php
function outer(string $value): void {
    echo $value;
    $plain = function (): void {
        $value = 'local';
        echo $value;
        $child = fn (): string => $value;
    };
    echo $value;
}
"#;
    let (line, col) = find_line_col(code, "$value = 'local'");
    let refs = find_var_refs_at(code, line, col + 1, true);
    let lines: Vec<u32> = refs.iter().map(|reference| reference.range.0).collect();
    assert_eq!(lines, vec![4, 5, 6]);

    let reads = find_var_refs_at(code, line, col + 1, false);
    let lines: Vec<u32> = reads.iter().map(|reference| reference.range.0).collect();
    assert_eq!(lines, vec![5, 6]);
}

#[test]
fn test_arrow_parameter_shadows_outer_and_nested_arrow_captures_parameter() {
    let code = r#"<?php
function outer(string $value): void {
    echo $value;
    $arrow = fn (
        string $value
    ): callable => fn (): string => $value;
    echo $value;
}
"#;
    let (line, col) = find_line_col(code, "        string $value");
    let refs = find_var_refs_at(code, line, col + "        string ".len() as u32 + 1, true);
    let lines: Vec<u32> = refs.iter().map(|reference| reference.range.0).collect();
    assert_eq!(lines, vec![4, 5]);

    let (outer_line, outer_col) = find_line_col(code, "echo $value;");
    let outer_refs = find_var_refs_at(code, outer_line, outer_col + "echo ".len() as u32 + 1, true);
    let lines: Vec<u32> = outer_refs
        .iter()
        .map(|reference| reference.range.0)
        .collect();
    assert_eq!(lines, vec![1, 2, 6]);
}

#[test]
fn test_anonymous_class_constructor_expression_keeps_outer_scope_but_method_does_not() {
    let code = r#"<?php
function outer(string $value): void {
    $object = new class($value) {
        public string $value;
        public function run(string $value): void {
            echo $value;
        }
    };
    echo $value;
}
"#;
    let (line, col) = find_line_col(code, "class($value)");
    let refs = find_var_refs_at(code, line, col + "class(".len() as u32 + 1, true);
    let lines: Vec<u32> = refs.iter().map(|reference| reference.range.0).collect();
    assert_eq!(
        lines,
        vec![1, 2, 8],
        "anonymous-class property and method scopes must remain isolated"
    );
}

#[test]
fn test_interpolated_variable_capture_respects_callable_scope() {
    let code = r#"<?php
function outer(string $value): void {
    echo "😀 {$value}";
    $captured = fn (): string => "{$value}";
    $plain = function (): string {
        return "{$value}";
    };
}
"#;
    let (line, col) = find_line_col(code, "{$value}");
    let refs = find_var_refs_at(code, line, col + 3, true);
    let lines: Vec<u32> = refs.iter().map(|reference| reference.range.0).collect();
    assert_eq!(lines, vec![1, 2, 3]);
}

#[test]
fn test_variable_declaration_filter_covers_nontrivial_local_declarations() {
    let cases = [
        (
            r#"<?php
function run(string ...$values): void {
    echo count($values);
}
"#,
            "count($values)",
            2,
            1,
        ),
        (
            r#"<?php
function run(array $row): void {
    ['id' => $value] = $row;
    echo $value;
}
"#,
            "echo $value",
            2,
            1,
        ),
        (
            r#"<?php
function run(string $subject): void {
    preg_match('/x/', $subject, $matches);
    echo count($matches);
}
"#,
            "count($matches)",
            2,
            1,
        ),
        (
            r#"<?php
function run(string $subject): void {
    preg_match('/x/', $subject, /* before value */ $matches);
    echo count($matches);
}
"#,
            "count($matches)",
            2,
            1,
        ),
        (
            r#"<?php
function run(string $subject): void {
    preg_match('/x/', $subject, $matches /* after value */);
    echo count($matches);
}
"#,
            "count($matches)",
            2,
            1,
        ),
        (
            r#"<?php
function run(): void {
    try { throw new Exception(); } catch (Exception $error) {
        echo $error->getMessage();
    }
}
"#,
            "$error->getMessage",
            2,
            1,
        ),
    ];

    for (code, needle, all_count, read_count) in cases {
        let (line, col) = find_line_col(code, needle);
        let variable_offset = code
            .lines()
            .nth(line as usize)
            .unwrap()
            .get(col as usize..)
            .and_then(|suffix| suffix.find('$'))
            .unwrap_or(0) as u32;
        assert_eq!(
            find_var_refs_at(code, line, col + variable_offset + 1, true).len(),
            all_count,
            "all references for {needle}"
        );
        assert_eq!(
            find_var_refs_at(code, line, col + variable_offset + 1, false).len(),
            read_count,
            "read references for {needle}"
        );
    }
}

#[test]
fn test_assignment_and_output_target_nested_expressions_remain_reads() {
    let cases = [
        (
            r#"<?php
function run(array $arr, string $key): void {
    $arr[$key] = 1;
    echo $key;
}
"#,
            "$arr[$key]",
            "$key",
            2,
        ),
        (
            r#"<?php
function run(object $obj): void {
    $obj->property = 1;
    var_dump($obj);
}
"#,
            "$obj->property",
            "$obj",
            2,
        ),
        (
            r#"<?php
function run(string $subject, array $matches, string $key): void {
    preg_match('/x/', $subject, $matches[$key]);
    echo $key;
}
"#,
            "$matches[$key]",
            "$key",
            2,
        ),
        (
            r#"<?php
function run(string $name): void {
    $$name = 1;
    echo $name;
}
"#,
            "$$name = 1",
            "$name",
            2,
        ),
        (
            r#"<?php
function run(string $subject, string $name): void {
    preg_match('/x/', $subject, $$name);
    echo $name;
}
"#,
            "$$name);",
            "$name",
            2,
        ),
    ];

    for (code, line_needle, variable, expected_reads) in cases {
        let (line, line_col) = find_line_col(code, line_needle);
        let variable_col = code
            .lines()
            .nth(line as usize)
            .unwrap()
            .get(line_col as usize..)
            .and_then(|suffix| suffix.find(variable))
            .unwrap() as u32;
        let reads = find_var_refs_at(code, line, line_col + variable_col + 1, false);
        assert_eq!(
            reads.len(),
            expected_reads,
            "nested read {variable} in {line_needle}: {reads:?}"
        );
    }

    let matches_code = r#"<?php
function run(string $subject, string $key): void {
    preg_match('/x/', $subject, $matches[$key]);
    echo count($matches);
}
"#;
    let (line, col) = find_line_col(matches_code, "count($matches)");
    let reads = find_var_refs_at(matches_code, line, col + "count(".len() as u32 + 1, false);
    assert_eq!(reads.len(), 1, "output base remains a declaration");
}

#[test]
fn test_keyed_destructuring_keys_are_reads_and_values_are_bindings() {
    let cases = [
        r#"<?php
function run(array $row, string $key): void {
    [$key => $value] = $row;
    echo $key;
    echo $value;
}
"#,
        r#"<?php
function run(array $row, string $key): void {
    list($key => $value) = $row;
    echo $key;
    echo $value;
}
"#,
        r#"<?php
function run(array $row, string $key): void {
    [$key /* key */ => /* value */ $value] = $row;
    echo $key;
    echo $value;
}
"#,
    ];

    for code in cases {
        let (key_line, key_col) = find_line_col(code, "echo $key;");
        let key_reads = find_var_refs_at(code, key_line, key_col + "echo ".len() as u32 + 1, false);
        assert_eq!(
            key_reads.len(),
            2,
            "dynamic destructuring key plus echo are reads for:\n{code}"
        );

        let (value_line, value_col) = find_line_col(code, "echo $value;");
        let value_reads = find_var_refs_at(
            code,
            value_line,
            value_col + "echo ".len() as u32 + 1,
            false,
        );
        assert_eq!(
            value_reads.len(),
            1,
            "destructuring value remains a binding for:\n{code}"
        );
    }
}
