use super::*;
#[test]
fn cancelled_cst_collection_returns_no_partial_result_and_unwinds_scope_budgets() {
    let source="<?php function target() {} function outer(){ $f=function(){ $g=fn()=>target(); target(); }; target(); }";
    let mut parser = crate::parser::FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    let symbols = crate::symbols::extract_file_symbols(tree, source, "file:///p238.php");
    let expected = collect_symbol_references_in_file(tree, source, &symbols);
    let _budget = crate::resolve::budget::FileInferenceBudget::for_tree(tree.root_node());
    for stop in [1, 5, 20, 35] {
        let checks = std::cell::Cell::new(0);
        let cancelled = || {
            let count = checks.get() + 1;
            checks.set(count);
            count >= stop
        };
        assert!(collect_symbol_references_in_file_cancellable(
            tree, source, &symbols, None, None, &cancelled
        )
        .is_none());
        assert_eq!(
            collect_symbol_references_in_file(tree, source, &symbols),
            expected
        );
    }
}
