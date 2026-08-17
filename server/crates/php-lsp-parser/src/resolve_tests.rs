use super::*;
use crate::parser::FileParser;
use crate::symbols::extract_file_symbols;

fn parse_and_resolve(code: &str, line: u32, col: u32) -> Option<SymbolAtPosition> {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    symbol_at_position(tree, code, line, col, &file_symbols)
}

fn parse_and_resolve_with_laravel_optional(
    code: &str,
    line: u32,
    col: u32,
) -> Option<SymbolAtPosition> {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    let function_resolver = |function_name: &str| -> Option<ResolvedFunctionType> {
        (function_name == "optional").then(|| {
            ResolvedFunctionType::new(
                "($callback is null ? \\Illuminate\\Support\\Optional : mixed)",
            )
            .with_symbol_fqn("optional")
        })
    };
    symbol_at_position_with_full_resolvers(
        tree,
        code,
        line,
        col,
        &file_symbols,
        None,
        None,
        Some(&function_resolver),
    )
}

fn test_param(name: &str) -> php_lsp_types::ParamInfo {
    php_lsp_types::ParamInfo {
        name: name.to_string(),
        type_info: None,
        default_value: None,
        is_variadic: false,
        is_by_ref: false,
        is_promoted: false,
    }
}

fn defaulted_test_param(name: &str, default_value: &str) -> php_lsp_types::ParamInfo {
    let mut param = test_param(name);
    param.default_value = Some(default_value.to_string());
    param
}

fn parse_and_find_var_def(code: &str, line: u32, col: u32) -> Option<(u32, u32, u32, u32)> {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    variable_definition_at_position(tree, code, line, col)
}

fn parse_and_local_variable_names(code: &str, line: u32, col: u32) -> Vec<String> {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    local_variable_names_at_position(tree, code, line, col)
}

fn parse_and_infer_var_type_at(code: &str, line: u32, col: u32, var_name: &str) -> Option<String> {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    infer_variable_type_at_position(tree, code, &file_symbols, line, col, var_name)
}

fn parse_and_infer_var_type_info_at(
    code: &str,
    line: u32,
    col: u32,
    var_name: &str,
) -> Option<TypeInfo> {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    infer_variable_type_info_at_position(tree, code, &file_symbols, line, col, var_name)
}

fn parse_and_variable_hover_info(code: &str, line: u32, col: u32) -> Option<VariableHoverInfo> {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    variable_hover_info_at_position(tree, code, &file_symbols, line, col)
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
fn test_position_to_byte_handles_crlf() {
    let source = "<?php\r\n$first = 1;\r\n$second = $first;\r\n";
    let expected = source.find("$second").unwrap();

    assert_eq!(position_to_byte(source, 2, 0), expected);
}

#[test]
fn test_position_to_byte_converts_utf16_character() {
    let source = "<?php\n$emoji = \"😀\";\n$result = $emoji;\n";
    let expected = source.find("$result").unwrap();

    assert_eq!(position_to_byte(source, 2, 0), expected);
}

struct ObjectTypeResolveDepthReset {
    previous: usize,
}

impl ObjectTypeResolveDepthReset {
    fn set(value: usize) -> Self {
        let previous = OBJECT_TYPE_RESOLVE_DEPTH.with(|depth| {
            let previous = depth.get();
            depth.set(value);
            previous
        });
        Self { previous }
    }
}

impl Drop for ObjectTypeResolveDepthReset {
    fn drop(&mut self) {
        OBJECT_TYPE_RESOLVE_DEPTH.with(|depth| depth.set(self.previous));
    }
}

fn object_type_resolve_depth() -> usize {
    OBJECT_TYPE_RESOLVE_DEPTH.with(|depth| depth.get())
}

#[test]
fn test_object_type_resolve_depth_guard_restores_after_panic() {
    let _reset = ObjectTypeResolveDepthReset::set(MAX_OBJECT_TYPE_RESOLVE_DEPTH - 1);

    let result = std::panic::catch_unwind(|| {
        let _guard = ObjectTypeResolveDepthGuard::enter()
            .expect("guard should enter below max resolve depth");
        assert_eq!(object_type_resolve_depth(), MAX_OBJECT_TYPE_RESOLVE_DEPTH);
        std::panic::resume_unwind(Box::new("simulated object type resolver panic"));
    });

    assert!(result.is_err());
    assert_eq!(
        object_type_resolve_depth(),
        MAX_OBJECT_TYPE_RESOLVE_DEPTH - 1
    );

    let guard = ObjectTypeResolveDepthGuard::enter()
        .expect("next resolve attempt should start from the restored depth");
    assert_eq!(object_type_resolve_depth(), MAX_OBJECT_TYPE_RESOLVE_DEPTH);
    drop(guard);
    assert_eq!(
        object_type_resolve_depth(),
        MAX_OBJECT_TYPE_RESOLVE_DEPTH - 1
    );
}

#[test]
fn test_object_type_resolve_depth_guard_respects_max_depth() {
    let _reset = ObjectTypeResolveDepthReset::set(MAX_OBJECT_TYPE_RESOLVE_DEPTH);

    assert!(ObjectTypeResolveDepthGuard::enter().is_none());
    assert_eq!(object_type_resolve_depth(), MAX_OBJECT_TYPE_RESOLVE_DEPTH);
}

#[test]
fn test_resolve_class_name_with_use() {
    let code = "<?php\nuse App\\Service\\UserService;\n\nnew UserService();\n";
    // "UserService" in "new UserService()" is at line 3
    let result = parse_and_resolve(code, 3, 5);
    assert!(result.is_some());
    let sym = result.unwrap();
    assert_eq!(sym.fqn, "App\\Service\\UserService::__construct");
    assert_eq!(sym.ref_kind, RefKind::Constructor);
}

#[test]
fn test_resolve_repeated_aliases_in_bracketed_namespace_scopes() {
    let code = r#"<?php
namespace First {
    use Vendor\First\Service as Shared;
    new sHaReD();
}
namespace Second {
    use Vendor\Second\Service as Shared;
    new SHARED();
}
"#;

    let first = parse_and_resolve(code, 3, 9).unwrap();
    assert_eq!(first.fqn, r"Vendor\First\Service::__construct");

    let second = parse_and_resolve(code, 7, 9).unwrap();
    assert_eq!(second.fqn, r"Vendor\Second\Service::__construct");
}

#[test]
fn test_resolve_repeated_aliases_in_unbracketed_namespace_scopes() {
    let code = r#"<?php
namespace First;
use Vendor\First\Service as Shared;
new shared();

namespace Second;
use Vendor\Second\Service as Shared;
new SHARED();
"#;

    let first = parse_and_resolve(code, 3, 8).unwrap();
    assert_eq!(first.fqn, r"Vendor\First\Service::__construct");

    let second = parse_and_resolve(code, 7, 8).unwrap();
    assert_eq!(second.fqn, r"Vendor\Second\Service::__construct");
}

#[test]
fn test_resolve_explicit_namespace_relative_class_and_function_names() {
    let code = r#"<?php
namespace App\Feature;
function run(): void {
    new namespace\Model();
    namespace\helper();
}
"#;

    let class = parse_and_resolve(code, 3, 22).unwrap();
    assert_eq!(class.fqn, r"App\Feature\Model::__construct");

    let function = parse_and_resolve(code, 4, 16).unwrap();
    assert_eq!(function.fqn, r"App\Feature\helper");
    assert_eq!(function.ref_kind, RefKind::FunctionCall);
    assert!(!function.allows_global_fallback);
}

#[test]
fn test_resolve_top_level_namespace_relative_function_without_changing_scope() {
    let code = r#"<?php
namespace App\Feature;
namespace\helper();
"#;

    let function = parse_and_resolve(code, 2, 12).unwrap();
    assert_eq!(function.fqn, r"App\Feature\helper");
    assert_eq!(function.ref_kind, RefKind::FunctionCall);
    assert!(!function.allows_global_fallback);
}

#[test]
fn test_resolve_function_and_class_aliases_case_insensitively() {
    let code = r#"<?php
namespace App;
use Vendor\Package\Service as ImportedService;
use function Vendor\Package\helper as ImportedHelper;
new importedservice();
IMPORTEDHELPER();
"#;

    let class = parse_and_resolve(code, 4, 10).unwrap();
    assert_eq!(class.fqn, r"Vendor\Package\Service::__construct");

    let function = parse_and_resolve(code, 5, 8).unwrap();
    assert_eq!(function.fqn, r"Vendor\Package\helper");
    assert!(!function.allows_global_fallback);
}

#[test]
fn test_resolve_trait_use_clause_name_with_import_alias() {
    let code = r#"<?php
namespace App\Jobs;

use Illuminate\Bus\Batchable;
use Illuminate\Queue\InteractsWithQueue;
use Illuminate\Bus\Queueable;
use Illuminate\Queue\SerializesModels;

class DeleteMultipleVCard
{
    use Batchable, InteractsWithQueue, Queueable, SerializesModels;
}
"#;
    let cases = [
        ("Batchable,", "Illuminate\\Bus\\Batchable"),
        (
            "InteractsWithQueue,",
            "Illuminate\\Queue\\InteractsWithQueue",
        ),
        ("Queueable,", "Illuminate\\Bus\\Queueable"),
        ("SerializesModels;", "Illuminate\\Queue\\SerializesModels"),
    ];

    for (needle, expected_fqn) in cases {
        let (line, col) = find_line_col(code, needle);
        let sym = parse_and_resolve(code, line, col).expect("trait use name should resolve");

        assert_eq!(sym.ref_kind, RefKind::ClassName);
        assert_eq!(sym.fqn, expected_fqn);
    }
}

#[test]
fn test_resolve_function_call() {
    let code = "<?php\nnamespace App;\n\nstrlen('hello');\n";
    let result = parse_and_resolve(code, 3, 0);
    assert!(result.is_some());
    let sym = result.unwrap();
    assert_eq!(sym.ref_kind, RefKind::FunctionCall);
}

#[test]
fn test_resolve_symbol_after_emoji_uses_utf16_position() {
    let code = "<?php\n$emoji = \"😀\"; strlen('hello');\n";
    let result = parse_and_resolve(code, 1, 17).expect("strlen should resolve after emoji");

    assert_eq!(result.ref_kind, RefKind::FunctionCall);
    assert_eq!(result.name, "strlen");
}

#[test]
fn test_resolve_qualified_function_call_relative_to_current_namespace() {
    let code = r#"<?php
namespace App\Diagnostics;

App\Utils\helper();
"#;
    let result = parse_and_resolve(code, 3, 13);
    assert!(result.is_some());
    let sym = result.unwrap();
    assert_eq!(sym.ref_kind, RefKind::FunctionCall);
    assert_eq!(sym.fqn, r"App\Diagnostics\App\Utils\helper");
    assert!(!sym.allows_global_fallback);
}

#[test]
fn test_global_fallback_metadata_only_allows_unqualified_unimported_names() {
    let code = r#"<?php
namespace App;
use function Vendor\helper as ImportedHelper;
use const Vendor\FLAG as ImportedFlag;

plainFunction();
ImportedHelper();
echo PLAIN_FLAG;
echo ImportedFlag;
"#;

    let (line, col) = find_line_col(code, "plainFunction");
    let plain_function = parse_and_resolve(code, line, col + 2).unwrap();
    assert_eq!(plain_function.ref_kind, RefKind::FunctionCall);
    assert!(plain_function.allows_global_fallback);

    let (line, col) = find_line_col(code, "ImportedHelper();");
    let imported_function = parse_and_resolve(code, line, col + 2).unwrap();
    assert_eq!(imported_function.ref_kind, RefKind::FunctionCall);
    assert!(!imported_function.allows_global_fallback);

    let (line, col) = find_line_col(code, "PLAIN_FLAG");
    let plain_constant = parse_and_resolve(code, line, col + 2).unwrap();
    assert_eq!(plain_constant.ref_kind, RefKind::GlobalConstant);
    assert!(plain_constant.allows_global_fallback);

    let (line, col) = find_line_col(code, "echo ImportedFlag");
    let imported_constant = parse_and_resolve(code, line, col + 7).unwrap();
    assert_eq!(imported_constant.ref_kind, RefKind::GlobalConstant);
    assert!(!imported_constant.allows_global_fallback);
}

#[test]
fn test_resolve_mixed_group_use_clause_targets_and_kinds() {
    let code = r#"<?php
use Vendor\Package\{
    Thing as Alias,
    function helper as DoWork,
    const FLAG as LocalFlag
};
"#;

    for (needle, expected_fqn, expected_kind) in [
        (
            "Thing as Alias",
            r"Vendor\Package\Thing",
            RefKind::ClassName,
        ),
        (
            "helper as DoWork",
            r"Vendor\Package\helper",
            RefKind::FunctionCall,
        ),
        (
            "FLAG as LocalFlag",
            r"Vendor\Package\FLAG",
            RefKind::GlobalConstant,
        ),
    ] {
        let (line, col) = find_line_col(code, needle);
        let symbol = parse_and_resolve(code, line, col + 1)
            .unwrap_or_else(|| panic!("group-use target should resolve: {needle}"));
        assert_eq!(symbol.fqn, expected_fqn);
        assert_eq!(symbol.ref_kind, expected_kind);
    }
}

#[test]
fn test_qualified_function_and_constant_names_expand_only_namespace_aliases() {
    let code = r#"<?php
namespace App;
use Vendor\Package as Lib;
use const Vendor\FLAG as ConstAlias;
"#;
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");

    assert_eq!(
        resolve_function_name_pub(r"lib\helper", &file_symbols),
        r"Vendor\Package\helper"
    );
    assert_eq!(
        resolve_constant_name_pub(r"LIB\FLAG", &file_symbols),
        r"Vendor\Package\FLAG"
    );
    assert_eq!(
        resolve_constant_name_pub("ConstAlias", &file_symbols),
        r"Vendor\FLAG"
    );
    assert_eq!(
        resolve_constant_name_pub(r"ConstAlias\CHILD", &file_symbols),
        r"App\ConstAlias\CHILD"
    );
}

#[test]
fn test_local_method_type_resolution_is_ascii_case_insensitive() {
    let code = r#"<?php
namespace App;
class User { public string $name; }
class Service {
    public function build(): User { return new User(); }
    public function run() { return $this->BUILD()->name; }
}
"#;
    let (line, col) = find_line_col(code, "->name");
    let symbol = parse_and_resolve(code, line, col + 2).expect("property should resolve");
    assert_eq!(symbol.fqn, r"App\User::$name");
    assert_eq!(symbol.ref_kind, RefKind::PropertyAccess);
}

#[test]
fn test_phpdoc_type_resolution_isolates_repeated_same_namespace_blocks() {
    let code = r#"<?php
namespace App {
    use Vendor\First\Model as Shared;
    class FirstFactory {
        /** @return Shared */
        public function make() {}
    }
}
namespace App {
    use Vendor\Second\Model as Shared;
    class SecondFactory {
        /** @return Shared */
        public function make() {}
    }
}
"#;
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");

    for (method_fqn, expected_type) in [
        (r"App\FirstFactory::make", r"\Vendor\First\Model"),
        (r"App\SecondFactory::make", r"\Vendor\Second\Model"),
    ] {
        let method = file_symbols
            .symbols
            .iter()
            .find(|symbol| symbol.fqn == method_fqn)
            .unwrap_or_else(|| panic!("missing extracted method: {method_fqn}"));
        assert_eq!(
            symbol_effective_type_info(method, &file_symbols),
            Some(TypeInfo::Simple(expected_type.to_string()))
        );
    }
}

#[test]
fn test_infer_variable_type_after_negative_instanceof_guard() {
    let code = r#"<?php
namespace App\Repository;

use App\Entity\User;
use Symfony\Component\Security\Core\User\PasswordAuthenticatedUserInterface;

class UserRepository {
    public function upgradePassword(PasswordAuthenticatedUserInterface $user): void {
        if (!$user instanceof User) {
            throw new \LogicException();
        }

        $user->setPassword('secret');
    }
}
"#;
    let (line, col) = find_line_col(code, "setPassword");
    let result = parse_and_infer_var_type_at(code, line, col, "$user");

    assert_eq!(result.as_deref(), Some("App\\Entity\\User"));
}

#[test]
fn test_resolve_class_definition() {
    let code = "<?php\nnamespace App;\n\nclass Foo {\n}\n";
    // "Foo" is at line 3, col 6
    let result = parse_and_resolve(code, 3, 6);
    assert!(result.is_some());
    let sym = result.unwrap();
    assert_eq!(sym.name, "Foo");
    assert_eq!(sym.fqn, "App\\Foo");
}

#[test]
fn test_resolve_method_call_on_new() {
    // (new Foo())->increment(5)
    let code = "<?php\nnamespace App;\nuse App\\Foo;\n\n(new Foo())->increment(5);\n";
    // "increment" is at line 4, col 13
    let result = parse_and_resolve(code, 4, 13);
    assert!(
        result.is_some(),
        "Should resolve method call on new expression"
    );
    let sym = result.unwrap();
    assert_eq!(sym.name, "increment");
    assert_eq!(sym.ref_kind, RefKind::MethodCall);
    assert_eq!(sym.fqn, "App\\Foo::increment");
}

#[test]
fn test_conditional_return_uses_subject_parameter_position_for_positional_arguments() {
    let signature = Signature {
        params: vec![test_param("prefix"), test_param("abstract")],
        return_type: None,
    };
    let function_resolver = |function_name: &str| -> Option<ResolvedFunctionType> {
        (function_name == "App\\helper").then(|| {
            ResolvedFunctionType::with_signature(
                "($abstract is class-string<TClass> ? TClass : mixed)",
                Some(signature.clone()),
            )
        })
    };

    let unresolved_code = r#"<?php
namespace App;
class Backend {}
helper(Backend::class)->ping();
"#;
    let mut parser = FileParser::new();
    parser.parse_full(unresolved_code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, unresolved_code, "file:///test.php");
    let (line, col) = find_line_col(unresolved_code, "ping");
    let unresolved = symbol_at_position_with_full_resolvers(
        tree,
        unresolved_code,
        line,
        col,
        &file_symbols,
        None,
        None,
        Some(&function_resolver),
    )
    .expect("method symbol should be produced even when receiver type is unresolved");

    assert_eq!(unresolved.ref_kind, RefKind::MethodCall);
    assert_eq!(unresolved.fqn, "ping");

    let resolved_code = r#"<?php
namespace App;
class Backend {}
helper('service', Backend::class)->ping();
"#;
    let mut parser = FileParser::new();
    parser.parse_full(resolved_code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, resolved_code, "file:///test.php");
    let (line, col) = find_line_col(resolved_code, "ping");
    let resolved = symbol_at_position_with_full_resolvers(
        tree,
        resolved_code,
        line,
        col,
        &file_symbols,
        None,
        None,
        Some(&function_resolver),
    )
    .expect("second positional argument should match the conditional subject");

    assert_eq!(resolved.ref_kind, RefKind::MethodCall);
    assert_eq!(resolved.fqn, "App\\Backend::ping");
}

#[test]
fn test_conditional_return_uses_defaulted_subject_argument_when_omitted() {
    let response_signature = Signature {
        params: vec![defaulted_test_param("content", "null")],
        return_type: None,
    };
    let redirect_signature = Signature {
        params: vec![defaulted_test_param("to", "null")],
        return_type: None,
    };
    let function_resolver = |function_name: &str| -> Option<ResolvedFunctionType> {
        match function_name {
            "App\\response" => Some(ResolvedFunctionType::with_signature(
                "($content is null ? App\\ResponseFactory : App\\Response)",
                Some(response_signature.clone()),
            )),
            "App\\redirect" => Some(ResolvedFunctionType::with_signature(
                "($to is null ? App\\Redirector : App\\RedirectResponse)",
                Some(redirect_signature.clone()),
            )),
            _ => None,
        }
    };

    let code = r#"<?php
namespace App;
class JsonResponse {}
class ResponseFactory { public function json(): JsonResponse {} }
class Response { public function setContent(string $content): void {} }
class RedirectResponse { public function with(string $key, mixed $value): self {} }
class Redirector { public function route(string $name): RedirectResponse {} }

response()->json();
response('ok')->setContent('ok');
redirect()->route('home');
redirect('/home')->with('status', 'ok');
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");

    let (line, col) = find_line_col(code, "json");
    let response_factory_method = symbol_at_position_with_full_resolvers(
        tree,
        code,
        line,
        col,
        &file_symbols,
        None,
        None,
        Some(&function_resolver),
    )
    .expect("response() should resolve through the default null conditional branch");
    assert_eq!(response_factory_method.fqn, "App\\ResponseFactory::json");

    let (line, col) = find_line_col(code, "setContent");
    let response_method = symbol_at_position_with_full_resolvers(
        tree,
        code,
        line,
        col,
        &file_symbols,
        None,
        None,
        Some(&function_resolver),
    )
    .expect("response('ok') should resolve through the non-null conditional branch");
    assert_eq!(response_method.fqn, "App\\Response::setContent");

    let (line, col) = find_line_col(code, "route");
    let redirector_method = symbol_at_position_with_full_resolvers(
        tree,
        code,
        line,
        col,
        &file_symbols,
        None,
        None,
        Some(&function_resolver),
    )
    .expect("redirect() should resolve through the default null conditional branch");
    assert_eq!(redirector_method.fqn, "App\\Redirector::route");

    let (line, col) = find_line_col(code, "with");
    let redirect_response_method = symbol_at_position_with_full_resolvers(
        tree,
        code,
        line,
        col,
        &file_symbols,
        None,
        None,
        Some(&function_resolver),
    )
    .expect("redirect('/home') should resolve through the non-null conditional branch");
    assert_eq!(redirect_response_method.fqn, "App\\RedirectResponse::with");
}

#[test]
fn test_resolve_method_call_on_this() {
    let code = "<?php\nnamespace App;\n\nclass Foo {\n    public function bar(): void {\n        $this->baz();\n    }\n}\n";
    // "baz" in "$this->baz()" at line 5, col 16
    let result = parse_and_resolve(code, 5, 16);
    assert!(result.is_some(), "Should resolve method call on $this");
    let sym = result.unwrap();
    assert_eq!(sym.name, "baz");
    assert_eq!(sym.ref_kind, RefKind::MethodCall);
    assert_eq!(sym.fqn, "App\\Foo::baz");
}

#[test]
fn test_resolve_parent_scope_to_extended_class() {
    let code = r#"<?php
namespace App;

class Base {
    public function run(): void {}
}

class Child extends Base {
    public function test(): void {
        parent::run();
    }
}
"#;
    let (line, col) = find_line_col(code, "parent::run");

    let scope = parse_and_resolve(code, line, col).expect("parent scope should resolve");
    assert_eq!(scope.name, "parent");
    assert_eq!(scope.ref_kind, RefKind::ClassName);
    assert_eq!(scope.fqn, "App\\Base");

    let method_col = col + "parent::".len() as u32;
    let method = parse_and_resolve(code, line, method_col).expect("parent method should resolve");
    assert_eq!(method.name, "run");
    assert_eq!(method.ref_kind, RefKind::MethodCall);
    assert_eq!(method.fqn, "App\\Base::run");
}

#[test]
fn test_resolve_parent_scope_inside_anonymous_class() {
    let code = r#"<?php
namespace App;

class ControllerHelper {
    public function __construct() {}
}

class Outer {
    public function create(): object {
        return new class extends ControllerHelper {
            public function setContainer(): void {
                parent::__construct();
            }
        };
    }
}
"#;
    let (line, col) = find_line_col(code, "parent::__construct");

    let scope =
        parse_and_resolve(code, line, col).expect("anonymous class parent scope should resolve");
    assert_eq!(scope.name, "parent");
    assert_eq!(scope.ref_kind, RefKind::ClassName);
    assert_eq!(scope.fqn, "App\\ControllerHelper");

    let method_col = col + "parent::".len() as u32;
    let method = parse_and_resolve(code, line, method_col)
        .expect("anonymous class parent method should resolve");
    assert_eq!(method.name, "__construct");
    assert_eq!(method.fqn, "App\\ControllerHelper::__construct");
}

#[test]
fn test_resolve_property_access_on_this() {
    let code = "<?php\nnamespace App;\n\nclass Foo {\n    private string $name;\n    public function bar(): string {\n        return $this->name;\n    }\n}\n";
    // "name" in "$this->name" at line 6, col 22
    let result = parse_and_resolve(code, 6, 22);
    assert!(result.is_some(), "Should resolve property access on $this");
    let sym = result.unwrap();
    assert_eq!(sym.name, "name");
    assert_eq!(sym.fqn, "App\\Foo::$name");
    assert_eq!(sym.ref_kind, RefKind::PropertyAccess);
}

#[test]
fn test_resolve_fully_qualified() {
    let code = "<?php\n\\DateTime::createFromFormat('Y-m-d', '2024-01-01');\n";
    // \\DateTime at line 1
    let result = parse_and_resolve(code, 1, 1);
    assert!(result.is_some());
}

#[test]
fn test_resolve_method_call_on_variable_assigned_new() {
    let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(): void {\n        $baz = new Baz();\n        $baz->test();\n    }\n}\n";
    // "test" in "$baz->test()" at line 7, col 15
    let result = parse_and_resolve(code, 7, 15);
    assert!(
        result.is_some(),
        "Should resolve method on variable assigned via new"
    );
    let sym = result.unwrap();
    assert_eq!(sym.name, "test");
    assert_eq!(sym.ref_kind, RefKind::MethodCall);
    assert_eq!(sym.fqn, "App\\Test\\Baz::test");
}

#[test]
fn test_resolve_method_call_on_function_return_with_nullable_resolver_type_text() {
    let code = r#"<?php
namespace App\Controller;

class Handler {
    public function run(): void {
        makeUser()->getName();
    }
}
"#;
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        (class_fqn.is_empty() && member_name == "App\\Controller\\makeUser")
            .then(|| "?App\\Entity\\User".to_string())
    };
    let (line, col) = find_line_col(code, "getName");
    let result =
        symbol_at_position_with_resolver(tree, code, line, col, &file_symbols, Some(&resolver))
            .expect("method call should resolve through normalized resolver type text");
    assert_eq!(result.ref_kind, RefKind::MethodCall);
    assert_eq!(result.fqn, "App\\Entity\\User::getName");
}

#[test]
fn test_laravel_optional_proxy_resolves_wrapped_property_and_method_chain() {
    let code = r#"<?php
namespace App;

class Profile {
    public function touch(): void {}
}

class User {
    public string $two_factor_secret;
    public function profile(): Profile {
        return new Profile();
    }
}

function run(?User $user): void {
    optional($user)->two_factor_secret;
    Optional($user)->two_factor_secret;
    optional(value: $user)->two_factor_secret;
    optional($user, null)->two_factor_secret;
    optional($user, /* no callback */ null)->two_factor_secret;
    optional($user, null /* no callback */)->two_factor_secret;
    optional($user)->profile()->touch();
}
"#;

    let (line, col) = find_line_col(code, "optional($user)->two_factor_secret");
    let col = col + "optional($user)->".len() as u32;
    let property = parse_and_resolve_with_laravel_optional(code, line, col)
        .expect("optional($user)->property should resolve on wrapped object");
    assert_eq!(property.ref_kind, RefKind::PropertyAccess);
    assert_eq!(property.fqn, "App\\User::$two_factor_secret");

    let (line, col) = find_line_col(code, "Optional($user)->two_factor_secret");
    let col = col + "Optional($user)->".len() as u32;
    let mixed_case_property = parse_and_resolve_with_laravel_optional(code, line, col)
        .expect("Optional($user)->property should resolve on the Laravel helper");
    assert_eq!(mixed_case_property.ref_kind, RefKind::PropertyAccess);
    assert_eq!(mixed_case_property.fqn, "App\\User::$two_factor_secret");

    let (line, col) = find_line_col(code, "value: $user)->two_factor_secret");
    let col = col + "value: $user)->".len() as u32;
    let named_property = parse_and_resolve_with_laravel_optional(code, line, col)
        .expect("optional(value: $user)->property should resolve on wrapped object");
    assert_eq!(named_property.ref_kind, RefKind::PropertyAccess);
    assert_eq!(named_property.fqn, "App\\User::$two_factor_secret");

    let (line, col) = find_line_col(code, "$user, null)->two_factor_secret");
    let col = col + "$user, null)->".len() as u32;
    let null_callback_property = parse_and_resolve_with_laravel_optional(code, line, col)
        .expect("optional($user, null)->property should resolve on wrapped object");
    assert_eq!(null_callback_property.ref_kind, RefKind::PropertyAccess);
    assert_eq!(null_callback_property.fqn, "App\\User::$two_factor_secret");

    let (line, col) = find_line_col(code, "$user, /* no callback */ null)->two_factor_secret");
    let col = col + "$user, /* no callback */ null)->".len() as u32;
    let leading_comment_null_callback_property =
        parse_and_resolve_with_laravel_optional(code, line, col)
            .expect("optional($user, /*...*/ null)->property should resolve");
    assert_eq!(
        leading_comment_null_callback_property.fqn,
        "App\\User::$two_factor_secret"
    );

    let (line, col) = find_line_col(code, "$user, null /* no callback */)->two_factor_secret");
    let col = col + "$user, null /* no callback */)->".len() as u32;
    let trailing_comment_null_callback_property =
        parse_and_resolve_with_laravel_optional(code, line, col)
            .expect("optional($user, null /*...*/)->property should resolve");
    assert_eq!(
        trailing_comment_null_callback_property.fqn,
        "App\\User::$two_factor_secret"
    );

    let (line, col) = find_line_col(code, "touch()");
    let chained_method = parse_and_resolve_with_laravel_optional(code, line, col)
        .expect("method chain after optional($user) should resolve through wrapped object");
    assert_eq!(chained_method.ref_kind, RefKind::MethodCall);
    assert_eq!(chained_method.fqn, "App\\Profile::touch");
}

#[test]
fn test_laravel_optional_proxy_does_not_unwrap_when_callback_is_present() {
    let code = r#"<?php
namespace App;

class User {
    public string $two_factor_secret;
}

function run(?User $user): void {
    optional($user, fn (User $value) => $value)->two_factor_secret;
    optional($user, $колбэк)->two_factor_secret;
}
"#;

    let (line, col) = find_line_col(
        code,
        "optional($user, fn (User $value) => $value)->two_factor_secret",
    );
    let col = col + "optional($user, fn (User $value) => $value)->".len() as u32;
    let resolved = parse_and_resolve_with_laravel_optional(code, line, col)
        .expect("unresolved property reference should still be detected");
    assert_eq!(resolved.ref_kind, RefKind::PropertyAccess);
    assert_eq!(resolved.fqn, "$two_factor_secret");

    let (line, col) = find_line_col(code, "optional($user, $колбэк)->two_factor_secret");
    let col = col + "optional($user, $колбэк)->".len() as u32;
    let multibyte_callback = parse_and_resolve_with_laravel_optional(code, line, col)
        .expect("multibyte callback argument should not panic or unwrap optional");
    assert_eq!(multibyte_callback.ref_kind, RefKind::PropertyAccess);
    assert_eq!(multibyte_callback.fqn, "$two_factor_secret");
}

#[test]
fn test_laravel_optional_proxy_does_not_unwrap_shadowed_optional_function() {
    let local_code = r#"<?php
namespace App;

class User {
    public string $two_factor_secret;
}

function optional($value) {
    return $value;
}

function run(?User $user): void {
    optional($user)->two_factor_secret;
}
"#;
    let (line, col) = find_line_col(local_code, "optional($user)->two_factor_secret");
    let col = col + "optional($user)->".len() as u32;
    let local = parse_and_resolve_with_laravel_optional(local_code, line, col)
        .expect("shadowed optional property reference should still be detected");
    assert_eq!(local.ref_kind, RefKind::PropertyAccess);
    assert_eq!(local.fqn, "$two_factor_secret");

    let imported_code = r#"<?php
namespace App;

use function Vendor\Optional;

class User {
    public string $two_factor_secret;
}

function run(?User $user): void {
    optional($user)->two_factor_secret;
    Optional($user)->two_factor_secret;
}
"#;
    let (line, col) = find_line_col(imported_code, "optional($user)->two_factor_secret");
    let col = col + "optional($user)->".len() as u32;
    let imported = parse_and_resolve_with_laravel_optional(imported_code, line, col)
        .expect("imported optional property reference should still be detected");
    assert_eq!(imported.ref_kind, RefKind::PropertyAccess);
    assert_eq!(imported.fqn, "$two_factor_secret");

    let (line, col) = find_line_col(imported_code, "Optional($user)->two_factor_secret");
    let col = col + "Optional($user)->".len() as u32;
    let imported_mixed_case = parse_and_resolve_with_laravel_optional(imported_code, line, col)
        .expect("case-insensitive imported optional should still shadow Laravel optional");
    assert_eq!(imported_mixed_case.ref_kind, RefKind::PropertyAccess);
    assert_eq!(imported_mixed_case.fqn, "$two_factor_secret");
}

#[test]
fn test_laravel_optional_proxy_resolves_wrapped_ternary_assignment() {
    let code = r#"<?php
namespace App;

class ExpireDate {
    public function getTimestamp(): int {
        return 0;
    }
}

class Signature {
    public function getSignature(): string {
        return '';
    }

    public function getExpire(): ?ExpireDate {
        return null;
    }
}

function run(): void {
    $signature = true ? new Signature() : null;
    optional($signature)->getSignature();
    optional(optional($signature)->getExpire())->getTimestamp();
}
"#;

    let (line, col) = find_line_col(code, "getSignature");
    let signature_method = parse_and_resolve_with_laravel_optional(code, line, col)
        .expect("optional($signature)->getSignature should resolve after ternary assignment");
    assert_eq!(signature_method.ref_kind, RefKind::MethodCall);
    assert_eq!(signature_method.fqn, "App\\Signature::getSignature");

    let (line, col) = find_line_col(code, ")->getTimestamp");
    let col = col + ")->".len() as u32;
    let expire_method = parse_and_resolve_with_laravel_optional(code, line, col)
        .expect("nested optional()->getExpire()->getTimestamp should resolve after ternary");
    assert_eq!(expire_method.ref_kind, RefKind::MethodCall);
    assert_eq!(expire_method.fqn, "App\\ExpireDate::getTimestamp");
}

#[test]
fn test_laravel_optional_proxy_resolves_wrapped_static_finder_assignment() {
    let code = r#"<?php
namespace App;

class User {
    public string $two_factor_secret;

    public static function find(int $id): ?static {
        return null;
    }
}

function run(): void {
    $user = User::find(1);
    optional($user)->two_factor_secret;
}
"#;

    let (line, col) = find_line_col(code, "optional($user)->two_factor_secret");
    let col = col + "optional($user)->".len() as u32;
    let property = parse_and_resolve_with_laravel_optional(code, line, col)
        .expect("optional(User::find())->property should resolve through ?static");
    assert_eq!(property.ref_kind, RefKind::PropertyAccess);
    assert_eq!(property.fqn, "App\\User::$two_factor_secret");
}

#[test]
fn test_infer_foreach_value_from_resolver_generic_function_return_preserves_args() {
    let code = r#"<?php
namespace App\Controller;

function run(): void {
    foreach (loadUsers() as $user) {
        $user;
    }
}
"#;
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        (class_fqn.is_empty() && member_name == "App\\Controller\\loadUsers")
            .then(|| "App\\Support\\Collection<int, App\\Entity\\User>".to_string())
    };
    let (line, col) = find_line_col(code, "$user;");
    let inferred = infer_variable_type_at_position_with_resolver(
        tree,
        code,
        &file_symbols,
        line,
        col,
        "$user",
        &resolver,
    )
    .expect("foreach value type should preserve generic resolver return args");
    assert_eq!(inferred, "App\\Entity\\User");
}

#[test]
fn test_resolve_foreach_value_from_resolver_generic_member_return_preserves_absolute_args() {
    let code = r#"<?php
namespace App\Soap\Inbound\Handler;

use App\Entity\ReverseRequest;

final class CompleteHandler {
    public function update(ReverseRequest $reverseRequest): void {
        foreach ($reverseRequest->getReversePortingNumbers() as $portingNumber) {
            $portingNumber->getPhoneNumber();
        }
    }
}
"#;
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        (class_fqn == "App\\Entity\\ReverseRequest" && member_name == "getReversePortingNumbers")
            .then(|| {
                "Doctrine\\Common\\Collections\\Collection<int, App\\Entity\\ReversePortingNumber>"
                    .to_string()
            })
    };
    let (line, col) = find_line_col(code, "$portingNumber->getPhoneNumber");
    let result = symbol_at_position_with_resolver(
        tree,
        code,
        line,
        col + "$portingNumber->".len() as u32,
        &file_symbols,
        Some(&resolver),
    )
    .expect("foreach value should resolve through normalized generic member return");
    assert_eq!(result.ref_kind, RefKind::MethodCall);
    assert_eq!(
        result.fqn,
        "App\\Entity\\ReversePortingNumber::getPhoneNumber"
    );
}

#[test]
fn test_infer_variable_type_from_fully_qualified_new_expression() {
    let code = r#"<?php
namespace App;

function run(object $object, string $method): void
{
    $reflMethod = new \ReflectionMethod($object, $method);
    $reflMethod->
}
"#;
    let (line, col) = find_line_col(code, "$reflMethod->");
    let result = parse_and_infer_var_type_at(
        code,
        line,
        col + "$reflMethod->".len() as u32,
        "$reflMethod",
    );

    assert_eq!(result.as_deref(), Some("ReflectionMethod"));

    let result_inside_member_name = parse_and_infer_var_type_at(
        code,
        line,
        col + "$reflMethod->isSt".len() as u32,
        "$reflMethod",
    );
    assert_eq!(
        result_inside_member_name.as_deref(),
        Some("ReflectionMethod")
    );
}

#[test]
fn test_infer_variable_type_from_assignment_inside_elseif_branch() {
    let code = r#"<?php
namespace App;

function run(object $object, mixed $method): void
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
    }
}
"#;
    let (line, col) = find_line_col(code, "$reflMethod->");
    let result = parse_and_infer_var_type_at(
        code,
        line,
        col + "$reflMethod->".len() as u32,
        "$reflMethod",
    );

    assert_eq!(result.as_deref(), Some("ReflectionMethod"));
}

#[test]
fn test_infer_variable_type_from_completed_if_assignment() {
    let code = r#"<?php
namespace App;

class Session {
    public function get(): string { return ''; }
}

function run(bool $enabled): void
{
    $session = null;
    if ($enabled) {
        $session = new Session();
    }

    $session?->get();
}
"#;
    let (line, col) = find_line_col(code, "$session?->");
    let result =
        parse_and_infer_var_type_at(code, line, col + "$session?->".len() as u32, "$session");

    assert_eq!(result.as_deref(), Some("App\\Session"));
}

#[test]
fn test_infer_variable_type_from_completed_if_method_return_with_resolver() {
    let code = r#"<?php
namespace App;

use Symfony\Component\HttpFoundation\Request;

function run(Request $request, bool $enabled): void
{
    $session = null;
    if ($enabled) {
        $session = $request->getSession();
    }

    $session?->get();
}
"#;
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    let (line, col) = find_line_col(code, "$session?->");
    let resolver = |class_fqn: &str, member_name: &str| {
        (class_fqn == "Symfony\\Component\\HttpFoundation\\Request" && member_name == "getSession")
            .then(|| "Symfony\\Component\\HttpFoundation\\Session\\SessionInterface".to_string())
    };
    let result = infer_variable_type_at_position_with_resolver(
        tree,
        code,
        &file_symbols,
        line,
        col + "$session?->".len() as u32,
        "$session",
        &resolver,
    );

    assert_eq!(
        result.as_deref(),
        Some("Symfony\\Component\\HttpFoundation\\Session\\SessionInterface")
    );
}

#[test]
fn test_resolve_nullable_method_call_from_completed_if_assignment() {
    let code = r#"<?php
namespace App;

class Session {
    public function get(string $key): string { return ''; }
}

function run(bool $enabled): void
{
    $session = null;
    if ($enabled) {
        $session = new Session();
    }

    $session?->get('token');
}
"#;
    let (line, col) = find_line_col(code, "get('token')");
    let result = parse_and_resolve(code, line, col)
        .expect("nullable method call should resolve from completed if assignment");

    assert_eq!(result.fqn, "App\\Session::get");
}

#[test]
fn test_resolve_self_reassignment_rhs_does_not_recurse() {
    let code = r#"<?php
namespace App;

class Generator {
    public function randomBased(): self { return $this; }
    public function generateId(): void {}
}

class Demo {
    public function run(): void {
        $generator = new Generator();
        $generator = $generator->randomBased();
        $generator->generateId();
    }
}
"#;
    let (line, col) = find_line_col(code, "randomBased");
    let reassignment_call =
        parse_and_resolve(code, line, col).expect("self-reassignment RHS should resolve");
    assert_eq!(reassignment_call.fqn, "App\\Generator::randomBased");

    let (line, col) = find_line_col(code, "generateId");
    let later_call = parse_and_resolve(code, line, col).expect("later method call should resolve");
    assert_eq!(later_call.fqn, "App\\Generator::generateId");
}

#[test]
fn test_resolve_method_call_on_typed_parameter() {
    let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(Baz $baz2): void {\n        $baz2->test();\n    }\n}\n";
    // "test" in "$baz2->test()" at line 6, col 16
    let result = parse_and_resolve(code, 6, 16);
    assert!(result.is_some(), "Should resolve method on typed parameter");
    let sym = result.unwrap();
    assert_eq!(sym.name, "test");
    assert_eq!(sym.ref_kind, RefKind::MethodCall);
    assert_eq!(sym.fqn, "App\\Test\\Baz::test");
}

#[test]
fn test_resolve_property_access_on_typed_parameter() {
    let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(Baz $baz2): void {\n        echo $baz2->name;\n    }\n}\n";
    // "name" in "$baz2->name" at line 6, col 20
    let result = parse_and_resolve(code, 6, 20);
    assert!(
        result.is_some(),
        "Should resolve property on typed parameter"
    );
    let sym = result.unwrap();
    assert_eq!(sym.name, "name");
    assert_eq!(sym.fqn, "App\\Test\\Baz::$name");
    assert_eq!(sym.ref_kind, RefKind::PropertyAccess);
}

#[test]
fn test_resolve_property_access_on_self_typed_parameter() {
    let code = r#"<?php
namespace App;

final class PromotedSelfDefaults {
    public function __construct(
        public ?string $objectManager = null,
        public ?array $mapping = null,
    ) {}

    public function withDefaults(self $defaults): static {
        $clone = clone $this;
        $clone->objectManager ??= $defaults->objectManager;
        $clone->mapping ??= $defaults->mapping ?? [];
        return $clone;
    }
}
"#;

    let (line, col) = find_line_col(code, "$defaults->objectManager");
    let result = parse_and_resolve(code, line, col + "$defaults->".len() as u32)
        .expect("self typed parameter property access should resolve");

    assert_eq!(result.name, "objectManager");
    assert_eq!(result.fqn, "App\\PromotedSelfDefaults::$objectManager");
    assert_eq!(result.ref_kind, RefKind::PropertyAccess);
}

#[test]
fn test_resolve_method_call_on_variable_typed_by_inline_phpdoc_var() {
    let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(): void {\n        /** @var Baz $baz2 */\n        $baz2 = makeBaz();\n        $baz2->test();\n    }\n}\n";
    // "test" in "$baz2->test()" at line 8
    let result = parse_and_resolve(code, 8, 16);
    assert!(
        result.is_some(),
        "Should resolve method on variable typed by inline @var"
    );
    let sym = result.unwrap();
    assert_eq!(sym.name, "test");
    assert_eq!(sym.ref_kind, RefKind::MethodCall);
    assert_eq!(sym.fqn, "App\\Test\\Baz::test");
}

#[test]
fn test_inline_phpdoc_var_must_match_variable_name() {
    let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(): void {\n        /** @var Baz $other */\n        $baz2 = makeBaz();\n        $baz2->test();\n    }\n}\n";
    // No matching @var for $baz2, so it should not be force-resolved as Baz.
    let result = parse_and_resolve(code, 8, 16).expect("symbol should resolve");
    assert_ne!(result.fqn, "App\\Test\\Baz::test");
}

#[test]
fn test_unnamed_inline_phpdoc_var_applies_to_immediate_assignment() {
    let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(): void {\n        /** @var Baz */\n        $baz2 = makeBaz();\n        $baz2->test();\n    }\n}\n";
    let result = parse_and_resolve(code, 8, 16).expect("symbol should resolve");
    assert_eq!(result.fqn, "App\\Test\\Baz::test");
}

#[test]
fn test_unnamed_inline_phpdoc_var_does_not_apply_without_assignment() {
    let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nclass Bar {\n    public function greet(): void {\n        /** @var Baz */\n        consume($baz2);\n        $baz2->test();\n    }\n}\n";
    let result = parse_and_resolve(code, 8, 16).expect("symbol should resolve");
    assert_ne!(result.fqn, "App\\Test\\Baz::test");
}

#[test]
fn test_infer_variable_type_at_position_from_inline_phpdoc_var() {
    let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nfunction run(): void {\n    /** @var Baz $baz2 */\n    $baz2 = makeBaz();\n    $baz2->\n}\n";
    // Cursor is after "$baz2->"
    let inferred =
        parse_and_infer_var_type_at(code, 7, 11, "$baz2").expect("type should be inferred");
    assert_eq!(inferred, "App\\Test\\Baz");
}

#[test]
fn test_variable_hover_info_from_inline_phpdoc_var() {
    let code = "<?php\nnamespace App;\nuse App\\Test\\Baz;\n\nfunction run(): void {\n    /**\n     * Local baz variable.\n     * @var Baz $baz2\n     */\n    $baz2 = makeBaz();\n    $baz2->test();\n}\n";
    let info = parse_and_variable_hover_info(code, 10, 7).expect("hover info should exist");
    assert_eq!(info.variable_name, "$baz2");
    assert_eq!(info.type_display.as_deref(), Some("Baz"));
    assert_eq!(info.resolved_type_fqn.as_deref(), Some("App\\Test\\Baz"));
    assert!(info
        .phpdoc_comment
        .as_deref()
        .unwrap_or("")
        .contains("@var Baz $baz2"));
}

#[test]
fn test_resolve_foreach_value_from_phpdoc_generic_array() {
    let code = r#"<?php
namespace App;

use App\Entity\User;

function run(): void {
    /** @var array<int, User> $users */
    $users = loadUsers();
    foreach ($users as $user) {
        $user->getName();
    }
}
"#;
    let (line, col) = find_line_col(code, "$user->getName");
    let col = col + "$user->".len() as u32;
    let result = parse_and_resolve(code, line, col).expect("foreach value should resolve");
    assert_eq!(result.fqn, "App\\Entity\\User::getName");
    assert_eq!(result.ref_kind, RefKind::MethodCall);
}

#[test]
fn test_resolve_foreach_value_from_phpdoc_generic_namespace_relative_method_return() {
    let code = r#"<?php
namespace App;

class Repository {
    /** @return array<int, Entity\User> */
    public function users(): array { return []; }
}

function run(Repository $repository): void {
    foreach ($repository->users() as $user) {
        $user;
    }
}
"#;
    let (line, col) = find_line_col(code, "$user;");
    let inferred = parse_and_infer_var_type_at(code, line, col, "$user")
        .expect("foreach value type should be inferred");
    assert_eq!(inferred, "App\\Entity\\User");
}

#[test]
fn test_resolve_foreach_value_from_phpdoc_generic_alias_qualified_method_return() {
    let code = r#"<?php
namespace App\Models {
    class User {
        public function getName(): string { return ''; }
    }
}

namespace App {
    use App\Models as Model;

    class Repository {
        /** @return array<int, Model\User> */
        public function users(): array { return []; }
    }

    function run(Repository $repository): void {
        foreach ($repository->users() as $user) {
            $user->getName();
        }
    }
}
"#;
    let (line, col) = find_line_col(code, "$user->getName");
    let col = col + "$user->".len() as u32;
    let result = parse_and_resolve(code, line, col).expect("foreach value should resolve");
    assert_eq!(result.fqn, "App\\Models\\User::getName");
    assert_eq!(result.ref_kind, RefKind::MethodCall);
}

#[test]
fn test_infer_foreach_value_from_member_assigned_collection() {
    let code = r#"<?php
namespace App;

/**
 * @var array<int, \App\Entity\DataRequest> $pagination
 */
$pagination = [];
foreach ($pagination as $dr):
    $shown = $dr->numbers;
    foreach ($shown as $num):
        $num;
    endforeach;
endforeach;
"#;
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    let (line, col) = find_line_col(code, "$num;");
    let node = find_node_at_point(tree.root_node(), Point::new(line as usize, col as usize))
        .expect("variable node");
    let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        (class_fqn == "App\\Entity\\DataRequest" && member_name == "$numbers")
            .then(|| "array<int, string>".to_string())
    };
    let info = infer_variable_hover_info_at_node_with_resolvers(
        node,
        code,
        &file_symbols,
        node.start_byte(),
        "$num",
        Some(&resolver),
        None,
    )
    .expect("foreach value should infer from assigned member collection");

    assert_eq!(info.type_display.as_deref(), Some("string"));
}

#[test]
fn test_infer_foreach_value_from_member_assigned_plain_array() {
    let code = r#"<?php
namespace App;

/**
 * @var array<int, \App\Entity\DataRequest> $pagination
 */
$pagination = [];
foreach ($pagination as $dr):
    $shown = $dr->numbers;
    foreach ($shown as $num):
        $num;
    endforeach;
endforeach;
"#;
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    let (line, col) = find_line_col(code, "$num;");
    let node = find_node_at_point(tree.root_node(), Point::new(line as usize, col as usize))
        .expect("variable node");
    let resolver = |class_fqn: &str, member_name: &str| -> Option<String> {
        (class_fqn == "App\\Entity\\DataRequest" && member_name == "$numbers")
            .then(|| "array".to_string())
    };
    let info = infer_variable_hover_info_at_node_with_resolvers(
        node,
        code,
        &file_symbols,
        node.start_byte(),
        "$num",
        Some(&resolver),
        None,
    )
    .expect("foreach value should infer mixed from plain array");

    assert_eq!(info.type_display.as_deref(), Some("mixed"));
}

#[test]
fn test_infer_foreach_value_from_array_keys_after_array_write() {
    let code = r#"<?php
function run(array $numbers): void {
    $normalizedNumbers = [];
    foreach ($numbers as $number) {
        $normalizedNumber = preg_replace('/\D+/', '', is_scalar($number) ? (string)$number : '') ?? '';
        if ('' !== $normalizedNumber) {
            $normalizedNumbers[$normalizedNumber] = true;
        }
    }
    $numbers = array_keys($normalizedNumbers);
    foreach ($numbers as $phoneNumber) {
        $phoneNumber;
    }
}
"#;
    let (line, col) = find_line_col(code, "$phoneNumber;");
    let info =
        parse_and_variable_hover_info(code, line, col + 2).expect("foreach value should infer");

    assert_eq!(info.variable_name, "$phoneNumber");
    assert_eq!(info.type_display.as_deref(), Some("string"));
}

#[test]
fn test_array_write_rhs_self_reference_hover_does_not_recurse() {
    let code = r#"<?php
function run(array $subscribers, array $phoneNumbers): void {
    foreach ($subscribers as &$subscriber) {
        $subscriber['phoneNumbers'] = array_column($phoneNumbers, 'phoneNumber');
        if ('Company' === $subscriber['type'] && $subscriber['organizationName']) {
            $subscriber['displayName'] = $subscriber['organizationName'];
        } else {
            $subscriber['displayName'] = trim(\sprintf(
                '%s %s %s',
                $subscriber['lastName'] ?? '',
                $subscriber['firstName'] ?? '',
                $subscriber['patronymic'] ?? ''
            ));
        }
    }
}
"#;
    let (line, col) = find_line_col(code, "$subscriber['lastName']");
    let info = parse_and_variable_hover_info(code, line, col + 2)
        .expect("array-offset RHS self reference should infer without recursion");

    assert_eq!(info.variable_name, "$subscriber");
    assert!(
        info.type_display.as_deref().is_some(),
        "expected a usable type display for array-offset self reference"
    );
}

#[test]
fn test_resolve_foreach_key_and_value_from_phpdoc_generator() {
    let code = r#"<?php
namespace App;

class User {
    public function getName(): string { return ''; }
}

function run(): void {
    /** @var \Generator<string, User, mixed, void> $users */
    $users = loadUsers();
    foreach ($users as $id => $user) {
        $id;
        $user->getName();
    }
}
"#;
    let (user_line, user_col) = find_line_col(code, "getName");
    let result =
        parse_and_resolve(code, user_line, user_col).expect("generator value should resolve");
    assert_eq!(result.fqn, "App\\User::getName");
    assert_eq!(result.ref_kind, RefKind::MethodCall);

    let (id_line, id_col) = find_line_col(code, "$id;");
    let inferred = parse_and_infer_var_type_info_at(code, id_line, id_col + 2, "$id")
        .expect("generator key should infer");
    assert_eq!(inferred, TypeInfo::Simple("string".to_string()));
}

#[test]
fn test_resolve_array_map_style_callback_parameter_from_callable_signature() {
    let code = r#"<?php
namespace App;

class User {
    public function getName(): string { return ''; }
}

/**
 * @template TItem
 * @template TResult
 * @param callable(TItem): TResult $callback
 * @param array<int, TItem> $items
 * @return array<int, TResult>
 */
function map_values(callable $callback, array $items): array { return []; }

function run(): void {
    /** @var array<int, User> $users */
    $users = [];
    map_values(fn($user) => $user->getName(), $users);
    $user->getName();
}
"#;
    let (line, col) = find_line_col(code, "$user->getName(),");
    let result = parse_and_resolve(code, line, col + "$user->".len() as u32)
        .expect("callback parameter method should resolve");
    assert_eq!(result.fqn, "App\\User::getName");
    assert_eq!(result.ref_kind, RefKind::MethodCall);

    let (outside_line, outside_col) = find_line_col(code, "$user->getName();");
    let outside = parse_and_resolve(code, outside_line, outside_col + "$user->".len() as u32)
        .expect("outside method still has a syntactic symbol");
    assert_ne!(
        outside.fqn, "App\\User::getName",
        "closure parameter type must not leak into outer scope"
    );
}

#[test]
fn test_resolve_collection_callback_parameter_from_receiver_generic_signature() {
    let code = r#"<?php
namespace App;

class User {
    public function getName(): string { return ''; }
}

/**
 * @template TItem
 */
class Collection {
    /**
     * @template TResult
     * @param callable(TItem): TResult $callback
     * @return Collection<TResult>
     */
    public function map(callable $callback): self { return $this; }

    /**
     * @param callable(TItem): bool $callback
     * @return Collection<TItem>
     */
    public function filter(callable $callback): self { return $this; }
}

function run(): void {
    /** @var Collection<User> $users */
    $users = loadUsers();
    $users->map(fn($user) => $user->getName());
    $users->filter(function ($user): bool {
        return '' !== $user->getName();
    });
}
"#;
    let (map_line, map_col) = find_line_col(code, "$user->getName());");
    let map_result = parse_and_resolve(code, map_line, map_col + "$user->".len() as u32)
        .expect("map callback parameter method should resolve");
    assert_eq!(map_result.fqn, "App\\User::getName");

    let (filter_line, filter_col) = find_line_col(code, "$user->getName();");
    let filter_result = parse_and_resolve(code, filter_line, filter_col + "$user->".len() as u32)
        .expect("filter callback parameter method should resolve");
    assert_eq!(filter_result.fqn, "App\\User::getName");
}

#[test]
fn test_resolve_array_access_from_phpdoc_generic_array() {
    let code = r#"<?php
namespace App;

use App\Entity\User;

function run(): void {
    /** @var array<int, User> $users */
    $users = loadUsers();
    $users[0]->getName();
}
"#;
    let (line, col) = find_line_col(code, "getName");
    let result = parse_and_resolve(code, line, col).expect("array element should resolve");
    assert_eq!(result.fqn, "App\\Entity\\User::getName");
    assert_eq!(result.ref_kind, RefKind::MethodCall);
}

#[test]
fn test_resolve_array_shape_access_from_phpdoc_var() {
    let code = r#"<?php
namespace App;

use App\Entity\User;

function run(): void {
    /** @var array{'user': User} $row */
    $row = [];
    $row['user']->getName();
}
"#;
    let (line, col) = find_line_col(code, "getName");
    let result = parse_and_resolve(code, line, col).expect("array-shape element should resolve");
    assert_eq!(result.fqn, "App\\Entity\\User::getName");
    assert_eq!(result.ref_kind, RefKind::MethodCall);
}

#[test]
fn test_resolve_array_access_from_phpdoc_generic_method_return() {
    let code = r#"<?php
namespace App;

use App\Entity\User;

class UserRepository {
    /** @return array<int, User> */
    public function findAll() {
        return [];
    }
}

function run(UserRepository $repo): void {
    $repo->findAll()[0]->getName();
}
"#;
    let (line, col) = find_line_col(code, "getName");
    let result =
        parse_and_resolve(code, line, col).expect("generic method return item should resolve");
    assert_eq!(result.fqn, "App\\Entity\\User::getName");
    assert_eq!(result.ref_kind, RefKind::MethodCall);
}

#[test]
fn test_infer_variable_type_at_position_from_phpdoc_array_access_text() {
    let code = r#"<?php
namespace App;

use App\Entity\User;

function run(): void {
    /** @var list<User> $users */
    $users = loadUsers();
    $users[0]->
}
"#;
    let inferred = parse_and_infer_var_type_at(code, 8, 16, "$users[0]")
        .expect("array access object type should be inferred for completion");
    assert_eq!(inferred, "App\\Entity\\User");
}

#[test]
fn test_infer_nested_array_shape_type_info_from_phpdoc_access_text() {
    let code = r#"<?php
function run(): void {
    /** @var array{meta: array{city: string, zip?: int}} $row */
    $row = [];
    $row['meta']['
}
"#;
    let inferred = parse_and_infer_var_type_info_at(code, 4, 18, "$row['meta']")
        .expect("nested array shape should be inferred");
    let TypeInfo::ArrayShape(items) = inferred else {
        panic!("expected nested array shape");
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].key.as_deref(), Some("city"));
    assert_eq!(items[1].key.as_deref(), Some("zip"));
    assert!(items[1].optional);
}

#[test]
fn test_infer_array_shape_from_multiline_file_type_alias() {
    let code = r#"<?php
namespace App;

/**
 * @phpstan-type RowShape array{
 *   'user-id': int,
 *   meta: array{
 *     city: string,
 *   },
 * }
 */
use App\Entity\User;

function run(): void {
    /** @var RowShape $row */
    $row = [];
    $row['meta']['
}
"#;
    let (line, col) = find_line_col(code, "$row['meta']['");
    let inferred = parse_and_infer_var_type_info_at(
        code,
        line,
        col + "$row['meta']".len() as u32,
        "$row['meta']",
    )
    .expect("type alias array shape should be expanded for local inference");
    let TypeInfo::ArrayShape(items) = inferred else {
        panic!("expected nested alias array shape");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key.as_deref(), Some("city"));
}

#[test]
fn test_infer_literal_array_shape_type_info() {
    let code = r#"<?php
function run(): void {
    $row = ['foo' => 1, 'meta' => ['city' => 'Paris']];
    $row['meta']['
}
"#;
    let inferred = parse_and_infer_var_type_info_at(code, 3, 18, "$row['meta']")
        .expect("literal nested array shape should be inferred");
    let TypeInfo::ArrayShape(items) = inferred else {
        panic!("expected literal nested array shape");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].key.as_deref(), Some("city"));
}

#[test]
fn test_infer_variable_type_inside_positive_instanceof_branch() {
    let code = r#"<?php
namespace App\Repository;

use App\Entity\User;
use Symfony\Component\Security\Core\User\PasswordAuthenticatedUserInterface;

class UserRepository {
    public function upgradePassword(PasswordAuthenticatedUserInterface $user): void {
        if ($user instanceof User) {
            $user->setPassword('secret');
        }
    }
}
"#;
    let (line, col) = find_line_col(code, "setPassword");
    let result = parse_and_infer_var_type_at(code, line, col, "$user");
    assert_eq!(result.as_deref(), Some("App\\Entity\\User"));
}

#[test]
fn test_infer_variable_type_inside_positive_elseif_instanceof_branch() {
    let code = r#"<?php
namespace App\Repository;

use App\Entity\OAuth1User;
use App\Entity\OAuth2User;
use App\Contracts\SocialiteUser;

class UserRepository {
    public function createToken(SocialiteUser $socialite): void {
        if ($socialite instanceof OAuth1User) {
            $socialite->tokenSecret;
        } elseif ($socialite instanceof OAuth2User) {
            $socialite->refreshToken;
        }
    }
}
"#;
    let (line, col) = find_line_col(code, "refreshToken");
    let result = parse_and_infer_var_type_at(code, line, col, "$socialite");
    assert_eq!(result.as_deref(), Some("App\\Entity\\OAuth2User"));
}

#[test]
fn test_infer_variable_type_inside_positive_instanceof_conjunction_branch() {
    let code = r#"<?php
namespace App\Repository;

use App\Entity\AbstractProvider;
use App\Contracts\Provider;

class UserRepository {
    public function configure(Provider $provider, bool $enabled): void {
        if ($enabled && $provider instanceof AbstractProvider) {
            $provider->setHttpClient();
        }
    }
}
"#;
    let (line, col) = find_line_col(code, "setHttpClient");
    let result = parse_and_infer_var_type_at(code, line, col, "$provider");
    assert_eq!(result.as_deref(), Some("App\\Entity\\AbstractProvider"));
}

#[test]
fn test_positive_instanceof_narrowing_ignores_negated_condition_branch() {
    let code = r#"<?php
namespace App\Repository;

use App\Entity\OAuth2User;
use App\Contracts\SocialiteUser;

class UserRepository {
    public function createToken(SocialiteUser $socialite): void {
        if (!($socialite instanceof OAuth2User)) {
            $socialite->refreshToken;
        }
    }
}
"#;
    let (line, col) = find_line_col(code, "refreshToken");
    let result = parse_and_infer_var_type_at(code, line, col, "$socialite");
    assert_ne!(result.as_deref(), Some("App\\Entity\\OAuth2User"));
}

#[test]
fn test_positive_instanceof_narrowing_ignores_nested_call_argument() {
    let code = r#"<?php
namespace App\Repository;

use App\Entity\OAuth2User;
use App\Contracts\SocialiteUser;

function accepts(bool $value): bool { return $value; }

class UserRepository {
    public function createToken(SocialiteUser $socialite): void {
        if (accepts($socialite instanceof OAuth2User)) {
            $socialite->refreshToken;
        }
    }
}
"#;
    let (line, col) = find_line_col(code, "refreshToken");
    let result = parse_and_infer_var_type_at(code, line, col, "$socialite");
    assert_ne!(result.as_deref(), Some("App\\Entity\\OAuth2User"));
}

#[test]
fn test_resolve_property_access_type_from_property_phpdoc_var() {
    let code = r#"<?php
namespace App;

use App\Entity\User;

class Holder {
    /** @var User */
    private $user;

    public function run(): void {
        $this->user->getName();
    }
}
"#;
    let (line, col) = find_line_col(code, "getName");
    let result = parse_and_resolve(code, line, col).expect("property @var type should resolve");
    assert_eq!(result.fqn, "App\\Entity\\User::getName");
    assert_eq!(result.ref_kind, RefKind::MethodCall);
}

#[test]
fn test_resolve_property_vs_method_same_name() {
    let code = "<?php\nnamespace App\\Test;\n\nclass Baz {\n    public string $test = 'x';\n    public function test(): string { return 'ok'; }\n}\n\nfunction go(Baz $baz2): void {\n    echo $baz2->test;\n    $baz2->test();\n}\n";

    // Property access should resolve to Baz::$test
    let prop = parse_and_resolve(code, 9, 17).expect("property should resolve");
    assert_eq!(prop.ref_kind, RefKind::PropertyAccess);
    assert_eq!(prop.fqn, "App\\Test\\Baz::$test");

    // Method call should resolve to Baz::test
    let method = parse_and_resolve(code, 10, 12).expect("method should resolve");
    assert_eq!(method.ref_kind, RefKind::MethodCall);
    assert_eq!(method.fqn, "App\\Test\\Baz::test");
}

#[test]
fn test_resolve_class_constant_access() {
    let code = "<?php\nnamespace App;\n\nclass Foo {\n    public const VERSION = '1.0';\n    public function run(): string {\n        return self::VERSION;\n    }\n}\n";
    // VERSION in self::VERSION
    let result = parse_and_resolve(code, 6, 21);
    assert!(result.is_some(), "Should resolve class constant access");
    let sym = result.unwrap();
    assert_eq!(sym.ref_kind, RefKind::ClassConstant);
    assert_eq!(sym.fqn, "App\\Foo::VERSION");
}

#[test]
fn test_resolve_global_constant_reference() {
    let code = "<?php\nnamespace App;\n\nconst BUILD = 'dev';\n\necho BUILD;\n";
    let result = parse_and_resolve(code, 5, 5);
    assert!(result.is_some(), "Should resolve global constant usage");
    let sym = result.unwrap();
    assert_eq!(sym.ref_kind, RefKind::GlobalConstant);
    assert_eq!(sym.fqn, "App\\BUILD");
}

#[test]
fn test_find_variable_definition_assignment() {
    let code = "<?php\nfunction demo(): void {\n    $value = 1;\n    echo $value;\n}\n";
    // $value in echo $value;
    let def = parse_and_find_var_def(code, 3, 10).expect("definition should be found");
    // points to assignment L3
    assert_eq!(def.0, 2);
}

#[test]
fn test_find_variable_definition_parameter() {
    let code = "<?php\nfunction demo(string $name): void {\n    echo $name;\n}\n";
    // $name in echo $name;
    let def = parse_and_find_var_def(code, 2, 10).expect("parameter definition should be found");
    // points to parameter line
    assert_eq!(def.0, 1);
}

#[test]
fn test_find_variable_definition_foreach_value_usage() {
    let code = r#"<?php
function demo(array $items): void {
    foreach ($items as $item) {
        echo $item;
    }
}
"#;
    let (line, col) = find_line_col(code, "echo $item");
    let def = parse_and_find_var_def(code, line, col + "echo ".len() as u32 + 2)
        .expect("foreach value variable definition should be found");
    let (def_line, def_col) = find_line_col(code, "$item) {");
    assert_eq!(def.0, def_line);
    assert_eq!(def.1, def_col);
}

#[test]
fn test_find_variable_definition_foreach_value_declaration_points_to_itself() {
    let code = r#"<?php
function demo(array $items): void {
    foreach ($items as $item) {
        echo $item;
    }
}
"#;
    let (line, col) = find_line_col(code, "$item) {");
    let def = parse_and_find_var_def(code, line, col + 2)
        .expect("foreach value declaration should be its own definition");
    assert_eq!(def.0, line);
    assert_eq!(def.1, col);
}

#[test]
fn test_find_variable_definition_preg_match_output_argument() {
    let code = r#"<?php
function demo(string $value): void {
    if (!preg_match('/(?P<year>\d+)/', $value, $matches)) {
        return;
    }
    echo $matches['year'];
}
"#;
    let (line, col) = find_line_col(code, "$matches['year']");
    let def = parse_and_find_var_def(code, line, col + 2)
        .expect("preg_match output variable definition should be found");
    let (def_line, def_col) = find_line_col(code, "$matches))");
    assert_eq!(def.0, def_line);
    assert_eq!(def.1, def_col);
}

#[test]
fn test_local_variable_names_include_preg_match_output_argument() {
    let code = r#"<?php
function demo(string $value): void {
    if (!preg_match('/(?P<year>\d+)/', $value, $matches)) {
        return;
    }
    $mat
}
"#;
    let (line, col) = find_line_col(code, "$mat");
    let names = parse_and_local_variable_names(code, line, col + "$mat".len() as u32);
    assert!(
        names.iter().any(|name| name == "$matches"),
        "expected $matches in local variable names, got: {:?}",
        names
    );
}

#[test]
fn test_local_variable_names_include_variadic_parameter() {
    let code = r#"<?php
function collect(string &...$args): void {
    $cursorArgs
}
"#;
    let (line, col) = find_line_col(code, "$cursorArgs");
    let names = parse_and_local_variable_names(code, line, col + "$cursorArgs".len() as u32);

    assert!(
        names.iter().any(|name| name == "$args"),
        "expected variadic parameter in local variable names, got: {names:?}"
    );
}

#[test]
fn test_local_variable_names_do_not_cross_nested_named_callable_scopes() {
    let code = r#"<?php
function outer(string $outerParam): void {
    $outerLocal = 1;
    function nested(string $nestedParam): void {
        $nestedLocal = 2;
    }
    $cursorOuter
}
"#;
    let (line, col) = find_line_col(code, "$cursorOuter");
    let names = parse_and_local_variable_names(code, line, col + "$cursorOuter".len() as u32);

    for expected in ["$outerParam", "$outerLocal"] {
        assert!(
            names.iter().any(|name| name == expected),
            "expected {expected} in outer scope, got: {names:?}"
        );
    }
    for leaked in ["$nestedParam", "$nestedLocal"] {
        assert!(
            !names.iter().any(|name| name == leaked),
            "nested callable variable {leaked} must not leak, got: {names:?}"
        );
    }
}

#[test]
fn test_local_variable_names_include_only_explicit_anonymous_function_captures() {
    let code = r#"<?php
function outer(string $captured, string $notCaptured): void {
    $outerLocal = 1;
    $closure = function (bool $ownParam) use ($captured): void {
        $cursorClosure
    };
}
"#;
    let (line, col) = find_line_col(code, "$cursorClosure");
    let names = parse_and_local_variable_names(code, line, col + "$cursorClosure".len() as u32);

    for expected in ["$captured", "$ownParam"] {
        assert!(
            names.iter().any(|name| name == expected),
            "expected {expected} in anonymous function scope, got: {names:?}"
        );
    }
    for leaked in ["$notCaptured", "$outerLocal"] {
        assert!(
            !names.iter().any(|name| name == leaked),
            "uncaptured outer variable {leaked} must not leak, got: {names:?}"
        );
    }
}

#[test]
fn test_local_variable_names_include_arrow_function_auto_captures() {
    let code = r#"<?php
function outer(string $outerParam): void {
    $outerLocal = 1;
    $arrow = fn (bool $ownParam): string => $cursorArrow;
}
"#;
    let (line, col) = find_line_col(code, "$cursorArrow");
    let names = parse_and_local_variable_names(code, line, col + "$cursorArrow".len() as u32);

    for expected in ["$outerParam", "$outerLocal", "$ownParam"] {
        assert!(
            names.iter().any(|name| name == expected),
            "expected {expected} in arrow function scope, got: {names:?}"
        );
    }
    assert!(
        !names.iter().any(|name| name == "$arrow"),
        "the variable receiving an arrow function must not capture itself, got: {names:?}"
    );
}

#[test]
fn test_local_variable_names_follow_this_visibility_through_nested_callables() {
    let code = r#"<?php
class Subject {
    public function instanceMethod(): void {
        $cursorInstance;
    }

    public static function staticMethod(): void {
        $cursorStatic;
    }

    public function nestedClosures(): void {
        $closure = function (): void {
            $cursorClosure;
        };
        $staticClosure = static function (): void {
            $cursorStaticClosure;
        };
        $arrow = fn (): mixed => $cursorArrow;
        $staticArrow = static fn (): mixed => $cursorStaticArrow;
    }

    public static function closureInStaticMethod(): void {
        $closure = function (): void {
            $cursorClosureInStatic;
        };
    }
}

function plainFunction(): void {
    $cursorFunction;
}

$cursorGlobal;
"#;

    for (marker, expected) in [
        ("$cursorInstance", true),
        ("$cursorStatic", false),
        ("$cursorClosure", true),
        ("$cursorStaticClosure", false),
        ("$cursorArrow", true),
        ("$cursorStaticArrow", false),
        ("$cursorClosureInStatic", false),
        ("$cursorFunction", false),
        ("$cursorGlobal", false),
    ] {
        let (line, col) = find_line_col(code, marker);
        let names = parse_and_local_variable_names(code, line, col + marker.len() as u32);
        assert_eq!(
            names.iter().any(|name| name == "$this"),
            expected,
            "unexpected `$this` visibility at {marker}: {names:?}"
        );
    }
}

#[test]
fn test_local_variable_names_handle_nested_arrow_and_closure_captures() {
    let code = r#"<?php
function outer(string $outerVisible, string $outerHidden): void {
    $closure = function (string $closureParam) use ($outerVisible): void {
        $closureLocal = 1;
        $arrow = fn (string $arrowParam): string => $cursorNestedArrow;
    };

    $arrow = fn (string $arrowParam) => function (string $closureParam) use ($arrowParam): void {
        $cursorNestedClosure
    };
}
"#;

    let (arrow_line, arrow_col) = find_line_col(code, "$cursorNestedArrow");
    let arrow_names = parse_and_local_variable_names(
        code,
        arrow_line,
        arrow_col + "$cursorNestedArrow".len() as u32,
    );
    for expected in [
        "$outerVisible",
        "$closureParam",
        "$closureLocal",
        "$arrowParam",
    ] {
        assert!(
            arrow_names.iter().any(|name| name == expected),
            "expected {expected} in nested arrow scope, got: {arrow_names:?}"
        );
    }
    for leaked in ["$outerHidden", "$arrow"] {
        assert!(
            !arrow_names.iter().any(|name| name == leaked),
            "unexpected {leaked} in nested arrow scope: {arrow_names:?}"
        );
    }

    let (closure_line, closure_col) = find_line_col(code, "$cursorNestedClosure");
    let closure_names = parse_and_local_variable_names(
        code,
        closure_line,
        closure_col + "$cursorNestedClosure".len() as u32,
    );
    for expected in ["$arrowParam", "$closureParam"] {
        assert!(
            closure_names.iter().any(|name| name == expected),
            "expected {expected} in nested closure scope, got: {closure_names:?}"
        );
    }
    for leaked in ["$outerVisible", "$outerHidden"] {
        assert!(
            !closure_names.iter().any(|name| name == leaked),
            "unexpected {leaked} in nested closure scope: {closure_names:?}"
        );
    }
}

#[test]
fn test_local_variable_names_survive_non_ascii_and_incomplete_callable() {
    let code = r#"<?php
function incomplete(string $unicodeParam, string &$referenceParam, string &...$rest): void {
    echo "😀"; $unicodeLocal = 1; $cursorIncomplete; $afterCursor = 2;
"#;
    let (line, col) = find_line_col(code, "$cursorIncomplete");
    let names = parse_and_local_variable_names(code, line, col + "$cursorIncomplete".len() as u32);

    for expected in ["$unicodeParam", "$referenceParam", "$rest", "$unicodeLocal"] {
        assert!(
            names.iter().any(|name| name == expected),
            "expected {expected} in incomplete unicode scope, got: {names:?}"
        );
    }
    assert!(
        !names.iter().any(|name| name == "$afterCursor"),
        "declaration after a non-ASCII cursor must not be included: {names:?}"
    );
}

#[test]
fn test_resolve_global_constant_in_method_body() {
    let code = "<?php\nnamespace App;\n\nconst BUILD = 'dev';\n\nclass Demo {\n    public const VERSION = '1.0';\n\n    public function run(): string {\n        $value = BUILD;\n        return self::VERSION . $value;\n    }\n}\n";
    let sym = parse_and_resolve(code, 9, 17).expect("BUILD symbol should resolve");
    assert_eq!(sym.ref_kind, RefKind::GlobalConstant);
    assert_eq!(sym.fqn, "App\\BUILD");
}

#[test]
fn test_resolve_static_property_access_variants() {
    let code = "<?php\nnamespace App;\n\nclass User { public static string $var = 'u'; }\n\nclass Demo {\n    public static string $created = 'c';\n    public static string $var = 'd';\n\n    public function run(): void {\n        echo self::$created;\n        echo static::$var;\n        echo User::$var;\n    }\n}\n";

    let (l1, c1) = find_line_col(code, "self::$created");
    let self_prop = parse_and_resolve(code, l1, c1 + 8).expect("self::$created should resolve");
    assert_eq!(self_prop.ref_kind, RefKind::StaticPropertyAccess);
    assert_eq!(self_prop.fqn, "App\\Demo::$created");

    let (l2, c2) = find_line_col(code, "static::$var");
    let static_prop = parse_and_resolve(code, l2, c2 + 9).expect("static::$var should resolve");
    assert_eq!(static_prop.ref_kind, RefKind::StaticPropertyAccess);
    assert_eq!(static_prop.fqn, "App\\Demo::$var");

    let (l3, c3) = find_line_col(code, "User::$var");
    let user_prop = parse_and_resolve(code, l3, c3 + 7).expect("User::$var should resolve");
    assert_eq!(user_prop.ref_kind, RefKind::StaticPropertyAccess);
    assert_eq!(user_prop.fqn, "App\\User::$var");
}

#[test]
fn test_infer_property_type_from_assignments() {
    use crate::parser::FileParser;
    use crate::symbols::extract_file_symbols;

    let code = r#"<?php
namespace App\Tests;

use App\Service\TimerService;
use Doctrine\ORM\EntityManagerInterface;

class MyTest {
    private EntityManagerInterface $em;
    private TimerService $timerService;

    protected function setUp(): void {
        $this->em = $this->createStub(EntityManagerInterface::class);
        $this->timerService = $this->createStub(TimerService::class);
    }

    public function testSomething(): void {
        $this->em->method('findAll');
    }
}
"#;

    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "test://file");

    // createStub returns Stub type via the resolver
    let resolver = |_class_fqn: &str, member_name: &str| -> Option<String> {
        if member_name == "createStub" {
            Some("PHPUnit\\Framework\\MockObject\\Stub".to_string())
        } else {
            None
        }
    };

    let result = super::infer_property_type_from_assignments(
        tree,
        code,
        "em",
        &file_symbols,
        Some(&resolver),
    );
    assert_eq!(
        result,
        vec!["PHPUnit\\Framework\\MockObject\\Stub".to_string()]
    );

    let result2 = super::infer_property_type_from_assignments(
        tree,
        code,
        "timerService",
        &file_symbols,
        Some(&resolver),
    );
    assert_eq!(
        result2,
        vec!["PHPUnit\\Framework\\MockObject\\Stub".to_string()]
    );

    // Non-existent property should return empty vec
    let result3 = super::infer_property_type_from_assignments(
        tree,
        code,
        "nonexistent",
        &file_symbols,
        Some(&resolver),
    );
    assert!(result3.is_empty());
}

#[test]
fn test_resolve_use_statement_goto_def() {
    let code = "<?php\nuse Doctrine\\ORM\\EntityManagerInterface;\n";

    // Cursor on "EntityManagerInterface" — should resolve full FQN
    let result = parse_and_resolve(code, 1, 20).unwrap();
    assert_eq!(result.fqn, "Doctrine\\ORM\\EntityManagerInterface");
    assert_eq!(result.ref_kind, RefKind::ClassName);

    // Cursor on "Doctrine" (first segment)
    let result2 = parse_and_resolve(code, 1, 4).unwrap();
    assert_eq!(result2.fqn, "Doctrine\\ORM\\EntityManagerInterface");
    assert_eq!(result2.ref_kind, RefKind::ClassName);

    // Cursor on "ORM" (middle segment)
    let result3 = parse_and_resolve(code, 1, 13).unwrap();
    assert_eq!(result3.fqn, "Doctrine\\ORM\\EntityManagerInterface");
    assert_eq!(result3.ref_kind, RefKind::ClassName);

    // Single-segment use statement
    let code2 = "<?php\nuse TestCase;\n";
    let result4 = parse_and_resolve(code2, 1, 4).unwrap();
    assert_eq!(result4.fqn, "TestCase");
    assert_eq!(result4.ref_kind, RefKind::ClassName);
}

#[test]
fn test_resolve_new_qualified_name() {
    // new Assert\NotBlank — qualified name in object_creation_expression
    let code = r#"<?php
namespace App\Form;

use Symfony\Component\Validator\Constraints as Assert;

class Foo {
    public function build(): void {
        $x = new Assert\NotBlank(message: 'Test');
    }
}
"#;
    // Cursor on "NotBlank"
    let (l1, c1) = find_line_col(code, "Assert\\NotBlank");
    let result = parse_and_resolve(code, l1, c1 + 7).unwrap();
    assert_eq!(
        result.fqn,
        "Symfony\\Component\\Validator\\Constraints\\NotBlank::__construct"
    );
    assert_eq!(result.ref_kind, RefKind::Constructor);

    // Cursor on "Assert" (namespace part)
    let result2 = parse_and_resolve(code, l1, c1).unwrap();
    assert_eq!(
        result2.fqn,
        "Symfony\\Component\\Validator\\Constraints\\NotBlank::__construct"
    );
    assert_eq!(result2.ref_kind, RefKind::Constructor);
}

#[test]
fn test_resolve_closure_param_method_call() {
    // Method call on closure parameter with type hint
    let code = r#"<?php
namespace App\Form;

use App\Repository\CatalogRepository;

class Foo {
    public function build(): void {
        $fn = static function (CatalogRepository $repository) {
            return $repository->createQueryBuilder('item');
        };
    }
}
"#;
    // Cursor on "createQueryBuilder"
    let (l1, c1) = find_line_col(code, "createQueryBuilder");
    let result = parse_and_resolve(code, l1, c1).unwrap();
    assert_eq!(
        result.fqn,
        "App\\Repository\\CatalogRepository::createQueryBuilder"
    );
    assert_eq!(result.ref_kind, RefKind::MethodCall);
}

#[test]
fn test_resolve_closure_param_method_chain() {
    // Method call chain on closure parameter: $subscriber->getLastName()
    let code = r#"<?php
namespace App\Form;

use App\Entity\Subscriber;

class Foo {
    public function build(): void {
        $fn = static function (Subscriber $subscriber) {
            return $subscriber->getLastName();
        };
    }
}
"#;
    // Cursor on "getLastName"
    let (l1, c1) = find_line_col(code, "getLastName");
    let result = parse_and_resolve(code, l1, c1).unwrap();
    assert_eq!(result.fqn, "App\\Entity\\Subscriber::getLastName");
    assert_eq!(result.ref_kind, RefKind::MethodCall);
}

#[test]
fn test_resolve_method_chain_static_return_type() {
    // Method chain: $qb->orderBy(...)->addOrderBy(...)
    // orderBy() returns `static`, addOrderBy is on same class
    let code = r#"<?php
namespace App\ORM;

class QueryBuilder {
    public function orderBy(string $sort): static {
        return $this;
    }
    public function addOrderBy(string $sort): static {
        return $this;
    }
}

class Foo {
    public function test(): void {
        $qb = new QueryBuilder();
        $qb->orderBy('a')->addOrderBy('b');
    }
}
"#;
    // Cursor on "addOrderBy" in the chain
    let (l, c) = find_line_col(code, "addOrderBy('b')");
    let result = parse_and_resolve(code, l, c).unwrap();
    assert_eq!(result.fqn, "App\\ORM\\QueryBuilder::addOrderBy");
    assert_eq!(result.ref_kind, RefKind::MethodCall);

    // Cursor on "orderBy" — first in chain
    let (l2, c2) = find_line_col(code, "orderBy('a')");
    let result2 = parse_and_resolve(code, l2, c2).unwrap();
    assert_eq!(result2.fqn, "App\\ORM\\QueryBuilder::orderBy");
    assert_eq!(result2.ref_kind, RefKind::MethodCall);
}

#[test]
fn test_resolve_method_chain_phpdoc_return_this() {
    // Method chain where return type comes from PHPDoc @return $this
    let code = r#"<?php
namespace App\ORM;

class Builder {
    /** @return $this */
    public function where(string $cond) {
        return $this;
    }
    /** @return $this */
    public function setParameter(string $name, $value) {
        return $this;
    }
    /** @return $this */
    public function orderBy(string $sort) {
        return $this;
    }
}

class Foo {
    public function test(): void {
        $b = new Builder();
        $b->where('x')->setParameter('y', 1)->orderBy('z');
    }
}
"#;
    // Cursor on "orderBy" — 3rd in chain
    let (l, c) = find_line_col(code, "orderBy('z')");
    let result = parse_and_resolve(code, l, c).unwrap();
    assert_eq!(result.fqn, "App\\ORM\\Builder::orderBy");
    assert_eq!(result.ref_kind, RefKind::MethodCall);

    // Cursor on "setParameter" — 2nd in chain
    let (l2, c2) = find_line_col(code, "setParameter('y'");
    let result2 = parse_and_resolve(code, l2, c2).unwrap();
    assert_eq!(result2.fqn, "App\\ORM\\Builder::setParameter");
    assert_eq!(result2.ref_kind, RefKind::MethodCall);
}

#[test]
fn test_resolve_method_chain_cross_class_return() {
    // Chain where createQueryBuilder() returns a different class
    let code = r#"<?php
namespace App\ORM;

class QueryBuilder {
    public function orderBy(string $sort): static {
        return $this;
    }
    public function addOrderBy(string $sort): static {
        return $this;
    }
}

class EntityRepository {
    public function createQueryBuilder(string $alias): QueryBuilder {
        return new QueryBuilder();
    }
}

class Foo {
    public function test(): void {
        $er = new EntityRepository();
        $er->createQueryBuilder('s')->orderBy('a')->addOrderBy('b');
    }
}
"#;
    // Cursor on "addOrderBy" — 3rd level chain
    let (l, c) = find_line_col(code, "addOrderBy('b')");
    let result = parse_and_resolve(code, l, c).unwrap();
    assert_eq!(result.fqn, "App\\ORM\\QueryBuilder::addOrderBy");
    assert_eq!(result.ref_kind, RefKind::MethodCall);

    // Cursor on "orderBy" — 2nd level
    let (l2, c2) = find_line_col(code, "orderBy('a')");
    let result2 = parse_and_resolve(code, l2, c2).unwrap();
    assert_eq!(result2.fqn, "App\\ORM\\QueryBuilder::orderBy");
    assert_eq!(result2.ref_kind, RefKind::MethodCall);

    // Cursor on "createQueryBuilder" — 1st level
    let (l3, c3) = find_line_col(code, "createQueryBuilder('s')");
    let result3 = parse_and_resolve(code, l3, c3).unwrap();
    assert_eq!(
        result3.fqn,
        "App\\ORM\\EntityRepository::createQueryBuilder"
    );
    assert_eq!(result3.ref_kind, RefKind::MethodCall);
}
