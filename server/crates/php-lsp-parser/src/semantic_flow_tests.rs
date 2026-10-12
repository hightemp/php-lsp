use super::*;
use crate::{parser::FileParser, symbols::extract_file_symbols};

fn check(body: &str, undefined: bool) {
    let source = format!("<?php function run(bool $flag,array $items){{ {body} }}");
    let mut parser = FileParser::new();
    parser.parse_full(&source);
    let tree = parser.tree().unwrap();
    assert!(
        !tree.root_node().has_error(),
        "invalid fixture: {body}: {}",
        tree.root_node().to_sexp()
    );
    let symbols = extract_file_symbols(tree, &source, "file:///flow.php");
    let diagnostics = extract_semantic_diagnostics(tree, &source, &symbols, |_, _| None);
    let found = diagnostics
        .iter()
        .filter(|d| {
            d.kind == SemanticDiagnosticKind::UndefinedVariable
                && d.message == "Undefined variable: $x"
        })
        .collect::<Vec<_>>();
    assert_eq!(!found.is_empty(), undefined, "{body}: {diagnostics:?}");
    if undefined && body.contains("/*read*/") {
        let offset = source.find("/*read*/").unwrap() + "/*read*/".len();
        assert_eq!(
            found[0].range,
            (0, offset as u32, 0, (offset + 2) as u32),
            "{body}: {found:?}"
        );
    }
}

#[test]
fn opposite_branches_do_not_share_assignments() {
    check("if($flag){$x=1;}else{echo /*read*/$x;}", true);
}
#[test]
fn partial_assignment_is_not_definite_at_a_join() {
    check("if($flag){$x=1;} echo /*read*/$x;", true);
    check(
        "if($flag){$x=1;}elseif(!$flag){$x=2;} echo /*read*/$x;",
        true,
    );
}
#[test]
fn complete_branches_and_exiting_alternatives_define_a_value() {
    check("if($flag){$x=1;}else{$x=2;} echo $x;", false);
    check("if($flag){$x=1;}else{return;} echo $x;", false);
    check("if($flag){return;}else{$x=2;} echo $x;", false);
}
#[test]
fn coalescing_assignment_defines_its_target_after_rhs_evaluation() {
    check("$x ??= 1; echo $x;", false);
    check("$x ??= /*read*/$x; echo $x;", true);
    check("$flag && ($x ??= 1); echo /*read*/$x;", true);
}
#[test]
fn boolean_short_circuit_and_ternary_assignments_keep_separate_paths() {
    check("$flag && ($x=1); echo /*read*/$x;", true);
    check("$flag || ($x=1); echo /*read*/$x;", true);
    check("$flag ? ($x=1) : 2; echo /*read*/$x;", true);
    check("$flag ? ($x=1) : ($x=2); echo $x;", false);
    check("if($flag && ($x=1)){echo $x;}", false);
}
#[test]
fn probes_do_not_warn_and_successful_guards_refine_only_the_right_path() {
    check("isset($x); empty($x); echo $x ?? 0;", false);
    check("if(isset($x)){echo $x;}", false);
    check("if(!empty($x)){echo $x;}", false);
    check("if(empty($x)){echo /*read*/$x;}", true);
    check("if(!isset($x)){return;} echo $x;", false);
}
#[test]
fn foreach_bindings_are_only_guaranteed_inside_the_loop() {
    check("foreach($items as $x){echo $x;}", false);
    check("foreach($items as $x){} echo /*read*/$x;", true);
    check("foreach($items as [$x]){echo $x;}", false);
    check("foreach($items as [$x]){} echo /*read*/$x;", true);
}
#[test]
fn loops_distinguish_zero_iterations_do_while_and_break_paths() {
    check("while($flag){$x=1;} echo /*read*/$x;", true);
    check("for(;$flag;){$x=1;} echo /*read*/$x;", true);
    check("do{$x=1;}while($flag); echo $x;", false);
    check(
        "do{if($flag)break;$x=1;}while(false); echo /*read*/$x;",
        true,
    );
    check("for($x=0;$flag;$x++){echo $x;} echo $x;", false);
}
#[test]
fn catch_bindings_do_not_leak_into_siblings_or_normal_completion() {
    check("try{}catch(Throwable $x){echo $x;} echo /*read*/$x;", true);
    check(
        "try{work();}catch(RuntimeException $x){}catch(Throwable $e){echo /*read*/$x;}",
        true,
    );
    check(
        "try{$x=work();}catch(Throwable $e){} echo /*read*/$x;",
        true,
    );
}
#[test]
fn finally_definitions_apply_to_surviving_paths() {
    check("try{work();}finally{$x=1;} echo $x;", false);
    check("try{if($flag)return;$x=1;}finally{echo /*read*/$x;}", true);
}
#[test]
fn destructuring_writes_happen_after_rhs_and_only_on_executed_paths() {
    check("[$x]=[/*read*/$x];", true);
    check("if($flag){[$x]=[1];} echo /*read*/$x;", true);
    check("[$x]=[1]; echo $x;", false);
}
#[test]
fn unset_removes_definite_bindings() {
    check("$x=1; unset($x); echo /*read*/$x;", true);
    check("$x=1; if($flag)unset($x); echo /*read*/$x;", true);
    check("unset($x);", false);
}
#[test]
fn captures_and_compact_observe_the_creation_path() {
    check("if($flag){$x=1;}else{$f=fn()=>/*read*/$x;}", true);
    check("if($flag){$x=1;}else{$f=function()use($x){};}", true);
    check("if($flag){$x=1;}else{compact('x');}", true);
    check("$f=function()use(&$x){}; echo $x;", false);
}
#[test]
fn array_keys_and_receivers_remain_reads_when_targets_are_written() {
    check("$items[/*read*/$x]=1;", true);
    check("[$items[/*read*/$x]]=[1];", true);
    check("$x->value=1;", true);
}
#[test]
fn switch_paths_include_fallthrough_and_a_missing_default() {
    check(
        "switch($flag){case true:$x=1;break;} echo /*read*/$x;",
        true,
    );
    check(
        "switch($flag){case true:$x=1;break;default:$x=2;} echo $x;",
        false,
    );
    check(
        "switch($flag){case true:$x=1;default:echo /*read*/$x;}",
        true,
    );
}

#[test]
fn by_reference_outputs_create_bindings_without_reading_them_first() {
    check("preg_match('/a/','a',$x); echo $x;", false);
    check("$flag && preg_match('/a/','a',$x); echo /*read*/$x;", true);
    check("$items =& $x; echo $x;", false);
}
#[test]
fn nullsafe_arguments_and_match_labels_are_conditional() {
    check("($items[0] ?? null)?->call($x=1); echo /*read*/$x;", true);
    check("match($flag){true,($x=1)=>/*read*/$x,default=>0};", true);
    check(
        "match($flag){true=>($x=1),default=>($x=2)}; echo $x;",
        false,
    );
}
#[test]
fn probe_boundaries_keep_indices_and_call_arguments_as_normal_reads() {
    check("isset($items[/*read*/$x]);", true);
    check("empty($items[/*read*/$x]);", true);
    check("echo consume(/*read*/$x) ?? 0;", true);
    check("isset($items); echo /*read*/$x;", true);
}
#[test]
fn alternative_syntax_and_exit_statements_preserve_reachability() {
    check("for(;$flag;):$items=[];echo /*read*/$x;endfor;", true);
    check("if($flag):$x=1;else:return;endif; echo $x;", false);
    check("if($flag){$x=1;}else{exit;} echo $x;", false);
    check("return; echo $x;", false);
}
#[test]
fn arrow_local_branch_writes_do_not_hide_an_outer_capture() {
    check("$f=fn()=>($flag ? ($x=1) : /*read*/$x);", true);
    check("$f=fn()=>($x ??=1)+$x;", false);
}
#[test]
fn flow_budgets_and_unsupported_jumps_do_not_use_the_old_textual_fallback() {
    check("goto done; $x=1; done: echo $x;", false);
    let body = format!("{} echo $x;", "if($flag){} ".repeat(20_000));
    check(&body, false);
}

#[test]
fn exception_edges_preserve_assignments_before_the_throwing_operation() {
    check(
        "try{$x=1;throw new Exception();}catch(Exception $e){echo $x;}",
        false,
    );
    check("try{$x=1;}finally{echo $x;}", false);
    check("try{work();$x=1;}finally{echo /*read*/$x;}", true);
    check("try{$x=work();}catch(Throwable $e){echo /*read*/$x;}", true);
}

#[test]
fn keyed_destructuring_defines_values_and_array_bases_after_key_reads() {
    check("[$flag=>$x]=[0=>1,1=>2]; echo $x;", false);
    check("[/*read*/$x=>$items[0]]=[0=>1];", true);
    check("[$x[0]]=[1]; echo $x;", false);
    check("[$x[$flag]]=[1]; echo $x;", false);
}
#[test]
fn exceptional_operations_and_halt_have_distinct_finally_paths() {
    check(
        "try{$items[0]=1/$flag;$x=1;}catch(Throwable $e){echo /*read*/$x;}",
        true,
    );
    check(
        "try{$x=1;$items[0]=1/$flag;}catch(Throwable $e){echo $x;}",
        false,
    );
    check("try{exit;}finally{echo $x;}", false);
}

#[test]
fn non_exhaustive_match_throws_into_its_catch() {
    check(
        "try{match(false){true=>($x=1)};}catch(UnhandledMatchError $e){echo /*read*/$x;}",
        true,
    );
}
#[test]
fn literal_arithmetic_does_not_create_an_impossible_catch_path() {
    check(
        "try{$items[0]=1+2;$x=1;}catch(Throwable $e){echo $x;}",
        false,
    );
}
#[test]
fn multiple_isset_arguments_short_circuit_their_side_effects() {
    check("isset($items[0],$items[$x=1]); echo /*read*/$x;", true);
}

#[test]
fn namespace_and_declare_bodies_keep_structured_flow() {
    check("declare(ticks=1){if($flag){$x=1;} echo /*read*/$x;}", true);
    let source =
        "<?php namespace App { $flag=$_GET['flag']??false; if($flag){$x=1;}else{echo $x;} }";
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    assert!(!tree.root_node().has_error());
    let symbols = extract_file_symbols(tree, source, "file:///namespace-flow.php");
    let diagnostics = extract_semantic_diagnostics(tree, source, &symbols, |_, _| None);
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic.kind
            == SemanticDiagnosticKind::UndefinedVariable
            && diagnostic.message == "Undefined variable: $x"),
        "{diagnostics:?}"
    );
}
