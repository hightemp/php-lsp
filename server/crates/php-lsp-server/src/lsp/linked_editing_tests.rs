use super::*;

fn ranges_at(source: &str, offset: usize) -> Option<Vec<(u32, u32, u32, u32)>> {
    let mut parser = FileParser::new();
    parser.parse_full(source);
    let tree = parser.tree().unwrap();
    let node = tree
        .root_node()
        .descendant_for_byte_range(offset, offset)
        .unwrap();
    linked_editing_ranges_for_namespace_or_use(source, node)
}

fn occurrences(source: &str, name: &str) -> Vec<usize> {
    source
        .match_indices(name)
        .map(|(offset, _)| offset)
        .collect()
}

fn expected(source: &str, offsets: &[usize], name: &str) -> Vec<(u32, u32, u32, u32)> {
    offsets
        .iter()
        .map(|offset| byte_offsets_to_range(source, *offset, *offset + name.len()))
        .collect()
}

fn assert_pair(source: &str, offsets: &[usize], name: &str) {
    let expected = expected(source, offsets, name);
    assert_eq!(expected.len(), 2);
    for offset in offsets {
        assert_eq!(
            ranges_at(source, offset + 1),
            Some(expected.clone()),
            "{source} at {offset}"
        );
    }
}

#[test]
fn linked_editing_keeps_redundant_alias_pairs_for_class_function_and_constant_imports() {
    for prefix in ["use", "use function", "use const"] {
        for target in ["Thing", "Vendor\\Thing", "\\Vendor\\Thing"] {
            let source = format!("<?php\n{prefix} {target} as Thing;");
            assert_pair(&source, &occurrences(&source, "Thing"), "Thing");
        }
    }
}

#[test]
fn linked_editing_links_only_the_terminal_target_and_alias_not_repeated_path_segments() {
    let source = "<?php use Thing\\Thing\\Thing as Thing;";
    let positions = occurrences(source, "Thing");
    for position in &positions[..2] {
        assert!(ranges_at(source, position + 1).is_none());
    }
    assert_pair(source, &positions[2..], "Thing");
}

#[test]
fn linked_editing_never_links_group_prefix_segments_to_clause_names() {
    let source = "<?php use Thing\\Thing\\{Thing as Thing, Other as Other};";
    let positions = occurrences(source, "Thing");
    for position in &positions[..2] {
        assert!(ranges_at(source, position + 1).is_none());
    }
    assert_pair(source, &positions[2..], "Thing");
    assert_pair(source, &occurrences(source, "Other"), "Other");
}

#[test]
fn linked_editing_keeps_same_terminal_names_in_distinct_import_clauses_independent() {
    for source in [
        "<?php use Vendor\\{First\\Thing as Thing, Second\\Thing as Local};",
        "<?php use First\\Thing as Thing, Second\\Thing as Local;",
        "<?php use Vendor\\{Thing as Thing, Thing as Local};",
    ] {
        let positions = occurrences(source, "Thing");
        assert_pair(source, &positions[..2], "Thing");
        assert!(ranges_at(source, positions[2] + 1).is_none());
        assert!(ranges_at(source, source.find("Local").unwrap() + 1).is_none());
    }
}

#[test]
fn linked_editing_rejects_ambiguous_effective_aliases_in_groups_and_comma_imports() {
    for source in [
        "<?php use Vendor\\{First\\Thing as Thing, Second\\Thing};",
        "<?php use Vendor\\{Thing as Thing, Other as Thing};",
        "<?php use Vendor\\{Thing as Thing, Other as thing};",
        "<?php use function Vendor\\{Thing as Thing, Other as thing};",
        "<?php use const Vendor\\{Thing as Thing, Other as Thing};",
        "<?php use First\\Thing as Thing, Second\\Thing;",
        "<?php use First\\Thing as Thing, Other as thing;",
    ] {
        for position in occurrences(source, "Thing") {
            assert!(ranges_at(source, position + 1).is_none(), "{source}");
        }
    }
}

#[test]
fn linked_editing_respects_separate_import_kinds_and_constant_alias_casing() {
    let source =
        "<?php use Vendor\\{Thing as Thing, function Thing as Thing, const Thing as Thing};";
    for pair in occurrences(source, "Thing").chunks_exact(2) {
        assert_pair(source, pair, "Thing");
    }
    let source = "<?php use const Vendor\\{Thing as Thing, Other as thing};";
    assert_pair(source, &occurrences(source, "Thing"), "Thing");
    assert!(ranges_at(source, source.find("thing").unwrap() + 1).is_none());
}

#[test]
fn linked_editing_rejects_repeated_namespace_segments_and_names_in_namespace_bodies() {
    for source in [
        "<?php namespace Thing\\Thing; class Thing {}",
        "<?php namespace Thing { class Thing {} function Thing() {} new Thing(); }",
        "<?php namespace Thing { } namespace Thing { class Thing {} }",
    ] {
        for position in occurrences(source, "Thing") {
            assert!(
                ranges_at(source, position + 1).is_none(),
                "{source} at {position}"
            );
        }
    }
    let source = "<?php namespace Thing { use Vendor\\Thing as Thing; class Thing {} }";
    let positions = occurrences(source, "Thing");
    assert!(ranges_at(source, positions[0] + 1).is_none());
    assert_pair(source, &positions[1..3], "Thing");
    assert!(ranges_at(source, positions[3] + 1).is_none());
}

#[test]
fn linked_editing_declines_malformed_imports_but_not_unrelated_source_errors() {
    for source in [
        "<?php use Vendor\\Thing as Thing",
        "<?php use Vendor\\{Thing as Thing, Other as };",
        "<?php use Vendor\\{Thing as Thing, Other;",
        "<?php use Vendor\\Thing as Thing, ;",
    ] {
        for position in occurrences(source, "Thing") {
            assert!(ranges_at(source, position + 1).is_none(), "{source}");
        }
    }
    let source = "<?php use Vendor\\Thing as Thing; $broken = ;";
    assert_pair(source, &occurrences(source, "Thing"), "Thing");
}

#[test]
fn linked_editing_ignores_trait_closure_usage_comment_string_and_punctuation_positions() {
    for source in [
        "<?php trait Thing {} class Demo { use Thing, Other { Thing::run as Thing; } }",
        "<?php $Thing = 1; $fn = function () use ($Thing) { return $Thing; };",
        "<?php namespace App { $Thing = 'Thing'; /* Thing */ new Thing(); }",
    ] {
        for position in occurrences(source, "Thing") {
            assert!(ranges_at(source, position + 1).is_none(), "{source}");
        }
    }
    let source = "<?php use Vendor\\Thing /* Thing */ as Thing;";
    let positions = occurrences(source, "Thing");
    assert_pair(source, &[positions[0], positions[2]], "Thing");
    assert!(ranges_at(source, positions[1] + 1).is_none());
    for token in ["use", "Vendor", "as", ";"] {
        assert!(
            ranges_at(source, source.find(token).unwrap()).is_none(),
            "{token}"
        );
    }
}

#[test]
fn linked_editing_requires_identical_spelling_and_an_explicit_alias() {
    for source in [
        "<?php use Vendor\\Thing;",
        "<?php use Vendor\\Thing as Local;",
        "<?php use Vendor\\Thing as thing;",
        "<?php use function Vendor\\Thing as thing;",
        "<?php use const Vendor\\Thing as thing;",
        "<?php use Vendor\\{First\\Thing, Second\\Thing};",
    ] {
        for position in occurrences(source, "Thing") {
            assert!(ranges_at(source, position + 1).is_none(), "{source}");
        }
    }
}
