use super::*;
use crate::parser::FileParser;
use crate::symbols::extract_file_symbols;
use php_lsp_types::{ParamInfo, PhpSymbolKind, Signature};

fn dummy_symbol() -> Arc<SymbolInfo> {
    Arc::new(SymbolInfo {
        name: String::new(),
        kind: PhpSymbolKind::Class,
        fqn: String::new(),
        range: (0, 0, 0, 0),
        selection_range: (0, 0, 0, 0),
        uri: String::new(),
        visibility: php_lsp_types::Visibility::Public,
        modifiers: Default::default(),
        attributes: vec![],
        doc_comment: None,
        signature: None,
        parent_fqn: None,
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    })
}

fn function_symbol(fqn: &str, params: Vec<ParamInfo>) -> Arc<SymbolInfo> {
    Arc::new(SymbolInfo {
        name: fqn.rsplit('\\').next().unwrap_or(fqn).to_string(),
        kind: PhpSymbolKind::Function,
        fqn: fqn.to_string(),
        range: (0, 0, 0, 0),
        selection_range: (0, 0, 0, 0),
        uri: String::new(),
        visibility: php_lsp_types::Visibility::Public,
        modifiers: Default::default(),
        attributes: vec![],
        doc_comment: None,
        signature: Some(Signature {
            params,
            return_type: None,
        }),
        parent_fqn: None,
        extends: vec![],
        implements: vec![],
        traits: vec![],
        templates: vec![],
        template_bindings: vec![],
    })
}

fn variadic_param(name: &str) -> ParamInfo {
    ParamInfo {
        name: name.to_string(),
        type_info: None,
        default_value: None,
        is_variadic: true,
        is_by_ref: false,
        is_promoted: false,
    }
}

fn compact_function_resolver(fqn: &str) -> Option<Arc<SymbolInfo>> {
    (fqn == "compact").then(|| function_symbol("compact", vec![variadic_param("var_name")]))
}

fn parse_and_check(
    code: &str,
    resolver: impl Fn(&str) -> Option<Arc<SymbolInfo>>,
) -> Vec<SemanticDiagnostic> {
    parse_and_check_typed(code, |fqn, expected_kinds| {
        resolver(fqn).map(|symbol| {
            if expected_kinds.contains(&symbol.kind) {
                symbol
            } else {
                let mut symbol = symbol.as_ref().clone();
                symbol.kind = expected_kinds[0];
                Arc::new(symbol)
            }
        })
    })
}

fn parse_and_check_typed(
    code: &str,
    resolver: impl Fn(&str, &[PhpSymbolKind]) -> Option<Arc<SymbolInfo>>,
) -> Vec<SemanticDiagnostic> {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    extract_semantic_diagnostics(tree, code, &file_symbols, resolver)
}

fn parse_and_check_with_file_resolver(code: &str) -> Vec<SemanticDiagnostic> {
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    let symbols = file_symbols.symbols.clone();
    extract_semantic_diagnostics(tree, code, &file_symbols, |fqn, expected_kinds| {
        symbols
            .iter()
            .find(|sym| sym.fqn == fqn && expected_kinds.contains(&sym.kind))
            .cloned()
            .map(Arc::new)
    })
}

#[test]
fn test_unknown_class_in_new() {
    let code = r#"<?php
namespace App;

use App\Service\UserService;

$x = new UserService();
$y = new UnknownClass();
"#;
    // UserService is known, UnknownClass is unknown
    let diags = parse_and_check(code, |fqn| {
        if fqn == "App\\Service\\UserService" {
            Some(dummy_symbol())
        } else {
            None
        }
    });

    let unknown: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == SemanticDiagnosticKind::UnknownClass)
        .collect();

    assert!(
        unknown.iter().any(|d| d.message.contains("UnknownClass")),
        "Expected unknown class diagnostic for UnknownClass, got: {:?}",
        unknown
    );

    // UserService should not be flagged
    assert!(
        !unknown.iter().any(|d| d.message.contains("UserService")),
        "UserService should be resolved, got: {:?}",
        unknown
    );
}

#[test]
fn test_unresolved_use() {
    let code = r#"<?php
namespace App;

use App\Service\UserService;
use App\Missing\SomeClass;
"#;
    let diags = parse_and_check(code, |fqn| {
        if fqn == "App\\Service\\UserService" {
            Some(dummy_symbol())
        } else {
            None
        }
    });

    let unresolved: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == SemanticDiagnosticKind::UnresolvedUse)
        .collect();

    assert_eq!(
        unresolved.len(),
        1,
        "Expected 1 unresolved use, got: {:?}",
        unresolved
    );
    assert!(unresolved[0].message.contains("App\\Missing\\SomeClass"));
}

#[test]
fn test_aliased_use_no_false_diagnostic() {
    // use ... as Alias; should NOT produce an unresolved diagnostic even
    // when the FQN doesn't resolve (it may be a namespace prefix import).
    let code = r#"<?php
namespace App;

use Symfony\Component\Validator\Constraints as Assert;
use App\Missing\SomeClass;
"#;
    let diags = parse_and_check(code, |_fqn| None);

    let unresolved: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == SemanticDiagnosticKind::UnresolvedUse)
        .collect();

    // Only the non-aliased one should be reported
    assert_eq!(
        unresolved.len(),
        1,
        "Expected 1 unresolved use (not the aliased one), got: {:?}",
        unresolved
    );
    assert!(unresolved[0].message.contains("App\\Missing\\SomeClass"));
    assert!(
        !unresolved.iter().any(|d| d.message.contains("Constraints")),
        "Aliased use statement should NOT be reported as unresolved"
    );
}

#[test]
fn test_unknown_namespaced_function() {
    let code = r#"<?php
namespace App;

App\Utils\helper();
"#;
    let diags = parse_and_check(code, |_fqn| None);

    let unknown_funcs: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == SemanticDiagnosticKind::UnknownFunction)
        .collect();

    // Should flag App\Utils\helper as unknown since it's namespaced
    assert!(
        !unknown_funcs.is_empty(),
        "Expected unknown function diagnostic for namespaced call"
    );
}

#[test]
fn test_semantic_resolution_rejects_wrong_top_level_symbol_kind() {
    let function_code = "<?php\nnamespace App;\nMissing();\n";
    let function_diags =
        parse_and_check_typed(function_code, |_fqn, _expected_kinds| Some(dummy_symbol()));
    assert!(function_diags.iter().any(|diagnostic| {
        diagnostic.kind == SemanticDiagnosticKind::UnknownFunction
            && diagnostic.message.contains("Missing")
    }));

    let class_code = "<?php\nnamespace App;\nnew Missing();\n";
    let class_diags = parse_and_check_typed(class_code, |fqn, _expected_kinds| {
        Some(function_symbol(fqn, Vec::new()))
    });
    assert!(class_diags.iter().any(|diagnostic| {
        diagnostic.kind == SemanticDiagnosticKind::UnknownClass
            && diagnostic.message.contains("Missing")
    }));
}

#[test]
fn test_semantic_resolution_selects_signature_for_legal_same_fqn_symbols() {
    let code = r#"<?php
new Collision();
Collision();
"#;
    let required_param = ParamInfo {
        name: "value".to_string(),
        type_info: None,
        default_value: None,
        is_variadic: false,
        is_by_ref: false,
        is_promoted: false,
    };
    let diags = parse_and_check_typed(code, |fqn, expected_kinds| {
        if expected_kinds.contains(&PhpSymbolKind::Function) {
            Some(function_symbol(fqn, vec![required_param.clone()]))
        } else if expected_kinds.contains(&PhpSymbolKind::Class) {
            let mut symbol = dummy_symbol().as_ref().clone();
            symbol.name = "Collision".to_string();
            symbol.fqn = fqn.to_string();
            Some(Arc::new(symbol))
        } else {
            None
        }
    });

    assert!(!diags.iter().any(|diagnostic| matches!(
        diagnostic.kind,
        SemanticDiagnosticKind::UnknownClass | SemanticDiagnosticKind::UnknownFunction
    )));
    assert!(diags.iter().any(|diagnostic| {
        diagnostic.kind == SemanticDiagnosticKind::ArgumentCountMismatch
            && diagnostic.message.contains("Collision")
    }));
}

#[test]
fn test_unknown_unqualified_function_after_namespace_and_global_fallbacks() {
    let code = r#"<?php
namespace App;

missing_helper();
"#;
    let diags = parse_and_check(code, |_fqn| None);

    let unknown_funcs: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == SemanticDiagnosticKind::UnknownFunction)
        .collect();

    assert_eq!(
        unknown_funcs.len(),
        1,
        "Expected one unknown function diagnostic, got: {:?}",
        unknown_funcs
    );
    assert!(unknown_funcs[0]
        .message
        .contains("Unknown function: App\\missing_helper"));
}

#[test]
fn test_language_construct_calls_do_not_report_unknown_functions_in_namespace() {
    let code = r#"<?php
namespace App\Controller;

$value = "code";
empty($value);
isset($value);
unset($value);
eval($value);
print($value);
exit($value);
die($value);
"#;
    let diags = parse_and_check(code, |_fqn| None);

    let unknown_funcs: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == SemanticDiagnosticKind::UnknownFunction)
        .collect();

    assert!(
        unknown_funcs.is_empty(),
        "Expected language constructs to avoid unknown function diagnostics, got: {:?}",
        unknown_funcs
    );
}

#[test]
fn test_unqualified_function_uses_current_namespace_or_global_fallback() {
    let code = r#"<?php
namespace App;

helper();
strlen("hello");
"#;
    let diags = parse_and_check(code, |fqn| {
        if fqn == "App\\helper" || fqn == "strlen" {
            Some(dummy_symbol())
        } else {
            None
        }
    });

    assert!(
        !diags
            .iter()
            .any(|d| d.kind == SemanticDiagnosticKind::UnknownFunction),
        "Expected namespace/global function fallback to avoid unknown diagnostics, got: {:?}",
        diags
    );
}

#[test]
fn test_imported_function_reports_import_fqn_when_missing() {
    let code = r#"<?php
namespace App;

use function Vendor\helper;

helper();
"#;
    let diags = parse_and_check(code, |_fqn| None);

    assert!(
        diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UnknownFunction
                && d.message.contains("Unknown function: Vendor\\helper")
        }),
        "Expected imported function FQN diagnostic, got: {:?}",
        diags
    );
}

#[test]
fn test_no_false_positives_for_builtins() {
    let code = r#"<?php
$x = new \stdClass();
strlen("hello");
array_map(fn($x) => $x, []);
"#;
    // All symbols are known (built-in)
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        diags.is_empty(),
        "Should have no diagnostics for built-in usage, got: {:?}",
        diags
    );
}

#[test]
fn test_function_argument_count_mismatch_too_few() {
    let code = r#"<?php
namespace App;

function helper(string $a, string $b): void {}
helper();
"#;
    let diags = parse_and_check(code, |fqn| {
        if fqn == "App\\helper" {
            Some(function_symbol(
                fqn,
                vec![
                    ParamInfo {
                        name: "a".to_string(),
                        type_info: None,
                        default_value: None,
                        is_variadic: false,
                        is_by_ref: false,
                        is_promoted: false,
                    },
                    ParamInfo {
                        name: "b".to_string(),
                        type_info: None,
                        default_value: None,
                        is_variadic: false,
                        is_by_ref: false,
                        is_promoted: false,
                    },
                ],
            ))
        } else {
            None
        }
    });

    let arg_diags: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == SemanticDiagnosticKind::ArgumentCountMismatch)
        .collect();

    assert!(
        arg_diags
            .iter()
            .any(|d| d.message.contains("Too few arguments to App\\helper()")),
        "Expected too-few-arguments diagnostic, got: {:?}",
        arg_diags
    );
}

#[test]
fn test_function_argument_count_mismatch_too_many() {
    let code = r#"<?php
namespace App;

function helper(string $a): void {}
helper("x", "y");
"#;
    let diags = parse_and_check(code, |fqn| {
        if fqn == "App\\helper" {
            Some(function_symbol(
                fqn,
                vec![ParamInfo {
                    name: "a".to_string(),
                    type_info: None,
                    default_value: None,
                    is_variadic: false,
                    is_by_ref: false,
                    is_promoted: false,
                }],
            ))
        } else {
            None
        }
    });

    let arg_diags: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == SemanticDiagnosticKind::ArgumentCountMismatch)
        .collect();

    assert!(
        arg_diags
            .iter()
            .any(|d| d.message.contains("Too many arguments to App\\helper()")),
        "Expected too-many-arguments diagnostic, got: {:?}",
        arg_diags
    );
}

#[test]
fn test_no_unknown_class_for_self_static_parent_type_hints() {
    let code = r#"<?php
namespace App;

class Base {}

class Child extends Base {
    public function withSelf(self $arg): static {
        return $this;
    }

    public function withParent(parent $arg): parent {
        return $arg;
    }
}
"#;
    let diags = parse_and_check(code, |fqn| {
        if fqn == "App\\Base" {
            Some(dummy_symbol())
        } else {
            None
        }
    });

    let unknown: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == SemanticDiagnosticKind::UnknownClass)
        .collect();

    assert!(
        unknown.is_empty(),
        "Expected no unknown-class diagnostics for self/static/parent, got: {:?}",
        unknown
    );
}

#[test]
fn test_no_unknown_class_for_case_insensitive_special_type_hints() {
    let code = r#"<?php
namespace App;

class Base {}

class Child extends Base {
    public function withSelf(Self $arg): STATIC {
        return $this;
    }

    public function withParent(PARENT $arg): PARENT {
        return $arg;
    }
}
"#;
    let diags = parse_and_check(code, |fqn| {
        if fqn == "App\\Base" {
            Some(dummy_symbol())
        } else {
            None
        }
    });

    let unknown: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == SemanticDiagnosticKind::UnknownClass)
        .collect();

    assert!(
        unknown.is_empty(),
        "Expected no unknown-class diagnostics for case-insensitive self/static/parent, got: {:?}",
        unknown
    );
}

/// Params after the first default-value param are implicitly optional even
/// without their own default value (common in phpstorm-stubs, e.g.
/// `preg_replace_callback`, `file_get_contents`).
#[test]
fn test_no_false_positive_for_optional_params_after_default() {
    // Simulates preg_replace_callback($pattern, $callback, $subject, int $limit = -1, &$count, int $flags = 0)
    // Only the first 3 params (before $limit which has a default) are truly required.
    let code = r#"<?php
preg_replace_callback('/x/', function(){}, 'input');
"#;
    let diags = parse_and_check(code, |fqn| {
        if fqn == "preg_replace_callback" {
            Some(function_symbol(
                fqn,
                vec![
                    ParamInfo {
                        name: "pattern".to_string(),
                        type_info: None,
                        default_value: None,
                        is_variadic: false,
                        is_by_ref: false,
                        is_promoted: false,
                    },
                    ParamInfo {
                        name: "callback".to_string(),
                        type_info: None,
                        default_value: None,
                        is_variadic: false,
                        is_by_ref: false,
                        is_promoted: false,
                    },
                    ParamInfo {
                        name: "subject".to_string(),
                        type_info: None,
                        default_value: None,
                        is_variadic: false,
                        is_by_ref: false,
                        is_promoted: false,
                    },
                    ParamInfo {
                        name: "limit".to_string(),
                        type_info: None,
                        default_value: Some("-1".to_string()),
                        is_variadic: false,
                        is_by_ref: false,
                        is_promoted: false,
                    },
                    ParamInfo {
                        name: "count".to_string(),
                        type_info: None,
                        default_value: None, // no default but after a defaulted param
                        is_variadic: false,
                        is_by_ref: true,
                        is_promoted: false,
                    },
                    ParamInfo {
                        name: "flags".to_string(),
                        type_info: None,
                        default_value: Some("0".to_string()),
                        is_variadic: false,
                        is_by_ref: false,
                        is_promoted: false,
                    },
                ],
            ))
        } else {
            None
        }
    });

    let arg_diags: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == SemanticDiagnosticKind::ArgumentCountMismatch)
        .collect();

    assert!(
            arg_diags.is_empty(),
            "Expected NO argument-count diagnostic for 3 args to preg_replace_callback (required prefix = 3), got: {:?}",
            arg_diags
        );
}

#[test]
fn test_unused_import_reports_only_unreferenced_alias() {
    let code = r#"<?php
namespace App;

use Vendor\UsedService;
use Vendor\UnusedService;

new UsedService();
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));
    let unused_imports: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == SemanticDiagnosticKind::UnusedImport)
        .collect();

    assert_eq!(
        unused_imports.len(),
        1,
        "Expected one unused import, got: {:?}",
        unused_imports
    );
    assert!(unused_imports[0].message.contains("Vendor\\UnusedService"));
}

#[test]
fn test_phpdoc_reference_counts_import_as_used() {
    let code = r#"<?php
namespace App;

use Random\RandomException;

class Generator {
    /**
     * @throws RandomException
     */
    public function run(): void {
    }
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        !diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UnusedImport
                && d.message.contains("Random\\RandomException")
        }),
        "PHPDoc type references should count as import usage, got: {:?}",
        diags
    );
}

#[test]
fn test_phpdoc_prose_does_not_count_import_as_used() {
    let code = r#"<?php
namespace App;

use Vendor\DocTextOnly;

class Generator {
    /**
     * @param string $value DocTextOnly appears only in prose.
     */
    public function run(string $value): void {
    }
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UnusedImport
                && d.message.contains("Vendor\\DocTextOnly")
        }),
        "PHPDoc prose should not count as import usage, got: {:?}",
        diags
    );
}

#[test]
fn test_phpdoc_type_does_not_count_function_import_as_used() {
    let code = r#"<?php
namespace App;

use function Vendor\DocType;

class Generator {
    /**
     * @param DocType $value
     */
    public function run($value): void {
    }
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UnusedImport && d.message.contains("Vendor\\DocType")
        }),
        "PHPDoc class-like types should not count as function import usage, got: {:?}",
        diags
    );
}

#[test]
fn test_undefined_variable_and_unused_local_diagnostics() {
    let code = r#"<?php
function run(string $used, string $unusedParam): void {
    echo $missing;
    $unusedLocal = 1;
    $usedLocal = 2;
    echo $used;
    echo $usedLocal;
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UndefinedVariable && d.message.contains("$missing")
        }),
        "Expected undefined variable diagnostic, got: {:?}",
        diags
    );
    assert!(
        diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UnusedParameter && d.message.contains("$unusedParam")
        }),
        "Expected unused parameter diagnostic, got: {:?}",
        diags
    );
    assert!(
        diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UnusedVariable && d.message.contains("$unusedLocal")
        }),
        "Expected unused local diagnostic, got: {:?}",
        diags
    );
    assert!(
        !diags.iter().any(|d| d.message.contains("$usedLocal"))
            && !diags.iter().any(|d| d.message.contains("$used")),
        "Used variables/params should not be reported, got: {:?}",
        diags
    );
}

#[test]
fn test_null_coalesce_probe_does_not_report_undefined_variable() {
    let code = r#"<?php
function run(): bool {
    return $maybeResult ?? false;
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        !diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UndefinedVariable
                && d.message.contains("$maybeResult")
        }),
        "Null coalesce left operand should not be reported as undefined, got: {:?}",
        diags
    );
}

#[test]
fn test_null_coalesce_probe_uses_nearest_left_or_right_operand() {
    let code = r#"<?php
function run(): void {
    echo $direct ?? null;
    echo $items['key'] ?? null;
    echo $outer ?? $inner ?? null;
    echo null ?? $rightMissing;
    echo ($defined ?? $nestedRightMissing) ?? null;
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));
    let is_undefined = |name: &str| {
        diags.iter().any(|diagnostic| {
            diagnostic.kind == SemanticDiagnosticKind::UndefinedVariable
                && diagnostic.message.contains(name)
        })
    };

    for suppressed in ["$direct", "$items", "$outer", "$inner", "$defined"] {
        assert!(
            !is_undefined(suppressed),
            "Left null-coalesce operand `{suppressed}` should be an isset-style probe, got: {diags:?}"
        );
    }
    for reported in ["$rightMissing", "$nestedRightMissing"] {
        assert!(
            is_undefined(reported),
            "Right null-coalesce operand `{reported}` should remain a normal read, got: {diags:?}"
        );
    }
}

#[test]
fn test_null_coalesce_probe_ignores_operator_text_in_strings_and_comments() {
    let code = r#"<?php
function run(): void {
    consume($stringMissing, "a??b");
    consume($commentMissing /* ?? */);
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    for expected in ["$stringMissing", "$commentMissing"] {
        assert!(
            diags.iter().any(|diagnostic| {
                diagnostic.kind == SemanticDiagnosticKind::UndefinedVariable
                    && diagnostic.message.contains(expected)
            }),
            "`??` source text must not hide undefined variable `{expected}`, got: {diags:?}"
        );
    }
}

#[test]
fn test_null_coalesce_assignment_left_operand_remains_a_probe() {
    let code = r#"<?php
function run(): void {
    $value ??= 1;
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        !diags.iter().any(|diagnostic| {
            diagnostic.kind == SemanticDiagnosticKind::UndefinedVariable
                && diagnostic.message.contains("$value")
        }),
        "Null-coalesce assignment left operand should remain an isset-style probe, got: {diags:?}"
    );
}

#[test]
fn test_compact_string_arguments_count_variables_as_reads() {
    let code = r#"<?php
function run(): array {
    $title = 'Extended service';
    $fields = [];
    $result = null;
    $names = ['extra'];

    return compact('title', ['fields', ['result']], $names);
}
"#;
    let diags = parse_and_check(code, compact_function_resolver);

    for variable in ["$title", "$fields", "$result", "$names"] {
        assert!(
            !diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UnusedVariable && d.message.contains(variable)
            }),
            "`{variable}` should be counted as read by compact(...), got: {:?}",
            diags
        );
    }
}

#[test]
fn test_namespaced_compact_string_arguments_count_variables_as_reads() {
    let code = r#"<?php
namespace App\Controller;

function run(): array {
    $title = 'Extended service';
    $fields = [];
    $result = null;

    return compact('title', ['fields', ['result']]);
}
"#;
    let diags = parse_and_check(code, compact_function_resolver);

    for variable in ["$title", "$fields", "$result"] {
        assert!(
            !diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UnusedVariable && d.message.contains(variable)
            }),
            "`{variable}` should be counted as read by namespaced compact(...), got: {:?}",
            diags
        );
    }
}

#[test]
fn test_global_compact_string_arguments_count_variables_as_reads() {
    let code = r#"<?php
namespace App\Controller;

function run(): array {
    $title = 'Extended service';

    return \compact('title');
}
"#;
    let diags = parse_and_check(code, compact_function_resolver);

    assert!(
        !diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UnusedVariable && d.message.contains("$title")
        }),
        "`$title` should be counted as read by \\compact(...), got: {:?}",
        diags
    );
}

#[test]
fn test_imported_compact_function_does_not_count_string_arguments_as_reads() {
    let code = r#"<?php
namespace App\Controller;

use function Vendor\compact;

function run(): array {
    $title = 'Extended service';

    return compact('title');
}
"#;
    let diags = parse_and_check(code, |fqn| {
        if fqn == "Vendor\\compact" {
            Some(function_symbol(fqn, vec![variadic_param("var_name")]))
        } else {
            None
        }
    });

    assert!(
        diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UnusedVariable && d.message.contains("$title")
        }),
        "Imported non-builtin compact(...) should not read `$title`, got: {:?}",
        diags
    );
}

#[test]
fn test_compact_import_from_another_namespace_does_not_hide_builtin_fallback() {
    let code = r#"<?php
namespace First;

use function Vendor\compact;

function first(): void {}

namespace Second;

function run(): array {
    $title = 'Extended service';
    return compact('title');
}
"#;
    let diags = parse_and_check(code, |fqn| {
        if fqn == "Vendor\\compact" {
            Some(function_symbol(fqn, vec![variadic_param("var_name")]))
        } else {
            None
        }
    });

    assert!(
        !diags.iter().any(|diagnostic| {
            diagnostic.kind == SemanticDiagnosticKind::UnusedVariable
                && diagnostic.message.contains("$title")
        }),
        "an import from another namespace must not mask builtin compact fallback: {diags:?}"
    );
}

#[test]
fn test_namespaced_compact_function_does_not_count_string_arguments_as_reads() {
    let code = r#"<?php
namespace App\Controller;

function compact(string ...$names): array
{
    return [];
}

function run(): array {
    $title = 'Extended service';

    return compact('title');
}
"#;
    let diags = parse_and_check_with_file_resolver(code);

    assert!(
        diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UnusedVariable && d.message.contains("$title")
        }),
        "Namespaced non-builtin compact(...) should not read `$title`, got: {:?}",
        diags
    );
}

#[test]
fn test_compact_string_argument_can_report_undefined_variable() {
    let code = r#"<?php
function run(): array {
    return compact('missing');
}
"#;
    let diags = parse_and_check(code, compact_function_resolver);

    assert!(
        diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UndefinedVariable && d.message.contains("$missing")
        }),
        "Undefined compact(...) variables should still be reported, got: {:?}",
        diags
    );
}

#[test]
fn test_arrow_function_auto_captures_outer_variables() {
    let code = r#"<?php
function run(): void {
    $npId = 'NP-1';
    $callback = static fn (array $context): bool => ($context['npId'] ?? null) === $npId;
    $callback([]);
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        !diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UndefinedVariable && d.message.contains("$npId")
        }),
        "Arrow functions should auto-capture outer variables, got: {:?}",
        diags
    );
}

#[test]
fn test_foreach_value_variable_is_declared() {
    let code = r#"<?php
function run(array $requests): void {
    foreach ($requests as $index => $request) {
        echo $index;
        echo $request;
    }
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        !diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UndefinedVariable && d.message.contains("$request")
        }),
        "foreach value variable should be declared, got: {:?}",
        diags
    );
    assert!(
        !diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UndefinedVariable && d.message.contains("$index")
        }),
        "foreach key variable should be declared, got: {:?}",
        diags
    );
}

#[test]
fn test_member_access_counts_variable_as_read() {
    let code = r#"<?php
function run(array $items): void {
    foreach ($items as $item) {
        echo $item->value;
    }
    $names = array_map(static fn ($case) => $case->name, $items);
    echo $names[0] ?? null;
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    for unexpected in ["$item", "$case"] {
        assert!(
            !diags.iter().any(|d| {
                (d.kind == SemanticDiagnosticKind::UnusedVariable
                    || d.kind == SemanticDiagnosticKind::UnusedParameter)
                    && d.message.contains(unexpected)
            }),
            "Member access receiver `{}` should count as a read, got: {:?}",
            unexpected,
            diags
        );
    }
}

#[test]
fn test_bodyless_method_parameters_are_not_unused() {
    let code = r#"<?php
interface Notifier {
    public function send(string $message, int $priority): void;
}

abstract class BaseHandler {
    abstract public function handle(object $message): array;
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        !diags
            .iter()
            .any(|d| d.kind == SemanticDiagnosticKind::UnusedParameter),
        "Interface/abstract declarations should not report unused params, got: {:?}",
        diags
    );
}

#[test]
fn test_override_unused_parameters_are_not_reported_without_name_hardcode() {
    let code = r#"<?php
namespace Vendor;

class BaseType {
    public function configure(object $builder, array $contractOnly = []): void {
        echo $builder;
        echo $contractOnly;
    }
}

interface VoteContract {
    public function voteOn(object $token, ?object $vote = null): bool;
}

namespace App;

class UserType extends \Vendor\BaseType {
    public function configure(object $builder, array $contractOnly = []): void {
        $builder->add('email');
    }
}

class ConcreteVote implements \Vendor\VoteContract {
    public function voteOn(object $token, ?object $vote = null): bool {
        echo $token;
        return true;
    }
}

class PlainType {
    public function buildForm(object $builder, array $options = []): void {
        echo $builder;
    }
}
"#;
    let diags = parse_and_check_with_file_resolver(code);

    for unexpected in ["$contractOnly", "$vote"] {
        assert!(
            !diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UnusedParameter && d.message.contains(unexpected)
            }),
            "Override parameter `{}` should not be reported, got: {:?}",
            unexpected,
            diags
        );
    }

    assert!(
            diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UnusedParameter && d.message.contains("$options")
            }),
            "Non-override `buildForm` must still report unused params; no method-name hardcode, got: {:?}",
            diags
        );
}

#[test]
fn test_preg_match_output_argument_declares_variable() {
    let code = r#"<?php
function run(string $content): void {
    if (preg_match('/<id>(\d+)<\/id>/', $content, $m)) {
        echo $m[1];
    }
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        !diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UndefinedVariable && d.message.contains("$m")
        }),
        "preg_match output variable should be declared, got: {:?}",
        diags
    );
}

#[test]
fn test_closure_use_by_reference_is_declared() {
    let code = r#"<?php
function run(): void {
    $persisted = null;
    $callback = function (object $entity) use (&$persisted): void {
        $persisted = $entity;
    };
    $callback(new stdClass());
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        !diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UndefinedVariable && d.message.contains("$persisted")
        }),
        "Closure use variables should be declared inside closures, got: {:?}",
        diags
    );
}

#[test]
fn test_closure_use_counts_as_outer_variable_read() {
    let code = r#"<?php
function run(): void {
    $callCount = 0;
    $callback = function () use (&$callCount): void {
        $callCount++;
    };
    $callback();
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        !diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UnusedVariable && d.message.contains("$callCount")
        }),
        "Closure use variables should count as reads in the outer scope, got: {:?}",
        diags
    );
}

#[test]
fn test_array_destructuring_assignment_declares_variables() {
    let code = r#"<?php
function pair(): array { return [1, 2]; }
function run(): void {
    [$left, $right] = pair();
    echo $left;
    echo $right;
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    for unexpected in ["$left", "$right"] {
        assert!(
            !diags.iter().any(|d| {
                d.kind == SemanticDiagnosticKind::UndefinedVariable
                    && d.message.contains(unexpected)
            }),
            "Array destructuring target `{}` should be declared, got: {:?}",
            unexpected,
            diags
        );
    }
}

#[test]
fn test_promoted_constructor_property_is_not_unused_parameter() {
    let code = r#"<?php
class Demo {
    public function __construct(private string $logger) {}
    public function run(): void {
        echo $this->logger;
    }
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));

    assert!(
        !diags.iter().any(|d| {
            d.kind == SemanticDiagnosticKind::UnusedParameter && d.message.contains("$logger")
        }),
        "Promoted constructor property should not be reported as unused parameter, got: {:?}",
        diags
    );
}

#[test]
fn test_duplicate_symbols_in_same_file() {
    let code = r#"<?php
namespace App;

class Duplicate {}
class Duplicate {}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));
    let duplicates: Vec<_> = diags
        .iter()
        .filter(|d| d.kind == SemanticDiagnosticKind::DuplicateSymbol)
        .collect();

    assert_eq!(
        duplicates.len(),
        2,
        "Expected both duplicate declarations to be reported, got: {:?}",
        duplicates
    );
    assert!(duplicates
        .iter()
        .all(|d| d.message.contains("App\\Duplicate")));
}

#[test]
fn test_duplicate_class_members_use_php_kind_specific_casing_rules() {
    let code = r#"<?php
class Owner {
    public function run(): void {}
    public function RUN(): void {}

    public string $value;
    public string $value;

    public const FLAG = 1;
    public const FLAG = 2;
}

enum Status {
    case Ready;
    case Ready;
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));
    let duplicates: Vec<_> = diags
        .iter()
        .filter(|diagnostic| diagnostic.kind == SemanticDiagnosticKind::DuplicateSymbol)
        .collect();

    assert_eq!(
        duplicates.len(),
        8,
        "unexpected diagnostics: {duplicates:?}"
    );
    assert!(duplicates
        .iter()
        .any(|diagnostic| diagnostic.message.contains("Owner::RUN")));
    assert!(duplicates
        .iter()
        .any(|diagnostic| diagnostic.message.contains("Status::Ready")));
}

#[test]
fn test_legal_member_overrides_are_not_duplicate_declarations() {
    let code = r#"<?php
class Base {
    public function run(): void {}
    public string $value;
    public const FLAG = 1;
}

class Child extends Base {
    public function RUN(): void {}
    public string $value;
    public const FLAG = 2;
}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));
    assert!(
        !diags
            .iter()
            .any(|diagnostic| { diagnostic.kind == SemanticDiagnosticKind::DuplicateSymbol }),
        "legal overrides must not be reported as duplicates: {diags:?}"
    );
}

#[test]
fn test_top_level_class_and_function_duplicates_ignore_ascii_case() {
    let code = r#"<?php
class MixedCase {}
class MIXEDCASE {}
function helper(): void {}
function HELPER(): void {}
"#;
    let diags = parse_and_check(code, |_fqn| Some(dummy_symbol()));
    let duplicates = diags
        .iter()
        .filter(|diagnostic| diagnostic.kind == SemanticDiagnosticKind::DuplicateSymbol)
        .count();
    assert_eq!(duplicates, 4, "unexpected diagnostics: {diags:?}");
}

#[test]
fn test_multi_namespace_diagnostics_use_the_local_namespace_scope() {
    let code = r#"<?php
namespace First {
    class Local {}
    new Local();
}
namespace Second {
    class Local {}
    new Local();
}
"#;
    let diags = parse_and_check(code, |fqn| {
        [r"First\Local", r"Second\Local"]
            .iter()
            .any(|known| known.eq_ignore_ascii_case(fqn))
            .then(dummy_symbol)
    });
    assert!(
        !diags
            .iter()
            .any(|diagnostic| { diagnostic.kind == SemanticDiagnosticKind::UnknownClass }),
        "namespace-local classes should resolve independently: {diags:?}"
    );
}

#[test]
fn test_namespace_relative_function_call_uses_current_namespace() {
    let code = r#"<?php
namespace App\Feature;
function run(): void {
    namespace\helper();
}
"#;
    let diags = parse_and_check(code, |fqn| {
        fqn.eq_ignore_ascii_case(r"App\Feature\helper")
            .then(dummy_symbol)
    });
    assert!(
        !diags
            .iter()
            .any(|diagnostic| { diagnostic.kind == SemanticDiagnosticKind::UnknownFunction }),
        "namespace-relative function should resolve locally: {diags:?}"
    );
}

#[test]
fn test_mixed_group_imports_resolve_with_per_clause_kinds() {
    let code = r#"<?php
use Vendor\Package\{
    Service as ImportedService,
    function helper as ImportedHelper,
    const FLAG as ImportedFlag
};

new importedservice();
IMPORTEDHELPER();
echo ImportedFlag;
"#;
    let diags = parse_and_check(code, |fqn| {
        [r"Vendor\Package\Service", r"Vendor\Package\helper"]
            .iter()
            .any(|known| known.eq_ignore_ascii_case(fqn))
            .then(dummy_symbol)
    });

    assert!(
        !diags.iter().any(|diagnostic| matches!(
            diagnostic.kind,
            SemanticDiagnosticKind::UnknownClass
                | SemanticDiagnosticKind::UnknownFunction
                | SemanticDiagnosticKind::UnusedImport
        )),
        "mixed group clauses should retain their own kinds: {diags:?}"
    );
}

#[test]
fn test_qualified_function_uses_namespace_alias_but_constant_alias_does_not_prefix() {
    let code = r#"<?php
namespace App;
use Vendor\Package as Lib;
use const Vendor\FLAG as ConstAlias;

lib\helper();
echo ConstAlias\CHILD;
"#;
    let diags = parse_and_check(code, |fqn| {
        fqn.eq_ignore_ascii_case(r"Vendor\Package\helper")
            .then(dummy_symbol)
    });

    assert!(!diags.iter().any(|diagnostic| {
        diagnostic.kind == SemanticDiagnosticKind::UnknownFunction
            || (diagnostic.kind == SemanticDiagnosticKind::UnusedImport
                && diagnostic.message.contains(r"Vendor\Package"))
    }));
    assert!(diags.iter().any(|diagnostic| {
        diagnostic.kind == SemanticDiagnosticKind::UnusedImport
            && diagnostic.message.contains(r"Vendor\FLAG")
    }));
}

#[test]
fn test_collect_aliased_class_fqns_is_scope_aware_and_case_insensitive() {
    let code = r#"<?php
namespace App {
    use Vendor\First as Shared;
    new shared\One();
}
namespace App {
    use Vendor\Second as Shared;
    new SHARED\Two();
}
"#;
    let mut parser = FileParser::new();
    parser.parse_full(code);
    let tree = parser.tree().unwrap();
    let file_symbols = extract_file_symbols(tree, code, "file:///test.php");
    let mut fqns = collect_aliased_class_fqns(tree, code, &file_symbols);
    fqns.sort();

    assert_eq!(fqns, vec![r"Vendor\First\One", r"Vendor\Second\Two"]);
}

#[test]
fn test_duplicate_global_constants_ignore_namespace_case_only() {
    let code = r#"<?php
namespace Vendor\Package { const FLAG = 1; }
namespace vendor\package { const FLAG = 2; const flag = 3; }
"#;
    let diags = parse_and_check(code, |_| None);
    let duplicates: Vec<_> = diags
        .iter()
        .filter(|diagnostic| diagnostic.kind == SemanticDiagnosticKind::DuplicateSymbol)
        .collect();

    assert_eq!(
        duplicates.len(),
        2,
        "only the two FLAG declarations collide"
    );
    assert!(duplicates
        .iter()
        .all(|diagnostic| diagnostic.message.ends_with(r"\FLAG")));
}
