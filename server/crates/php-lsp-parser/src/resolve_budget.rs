//! Synchronous inference work limits. Depth alone does not bound branching
//! fallback inference or repeated scans of preceding statements.

use std::cell::Cell;

pub(super) const MAX_INFERENCE_WORK: usize = 8_192;
const MAX_FILE_INFERENCE_WORK: usize = 131_072;

#[derive(Clone, Copy)]
struct Work {
    remaining: usize,
    file_remaining: Option<usize>,
    scope_remaining: Option<usize>,
    scope_quota: usize,
    limit_events: usize,
    exhausted: bool,
}

thread_local! {
    static WORK: Cell<Work> = const { Cell::new(Work {
        remaining: MAX_INFERENCE_WORK,
        file_remaining: None,
        scope_remaining:None,
        scope_quota:0,
        limit_events:0,
        exhausted: false,
    }) };
}

pub(super) fn begin() {
    WORK.with(|work| {
        let mut state = work.get();
        state.remaining = MAX_INFERENCE_WORK;
        state.exhausted = state.file_remaining == Some(0) || state.scope_remaining == Some(0);
        if state.exhausted && state.file_remaining.is_some() {
            state.limit_events += 1;
        }
        work.set(state);
    });
}

pub(super) fn consume() -> bool {
    WORK.with(|work| {
        let mut state = work.get();
        if state.exhausted {
            return false;
        }
        if state.remaining == 0
            || state.file_remaining == Some(0)
            || state.scope_remaining == Some(0)
        {
            state.exhausted = true;
            if state.file_remaining.is_some() {
                state.limit_events += 1;
            }
            work.set(state);
            return false;
        }
        state.remaining -= 1;
        if let Some(remaining) = &mut state.file_remaining {
            *remaining -= 1;
        }
        if let Some(remaining) = &mut state.scope_remaining {
            *remaining -= 1;
        }
        work.set(state);
        true
    })
}

pub(super) fn completed<T>(result: Option<T>) -> Option<T> {
    if WORK.with(|work| work.get().exhausted) {
        None
    } else {
        result
    }
}

pub(super) fn exhaust() {
    WORK.with(|work| {
        let mut state = work.get();
        if !state.exhausted && state.file_remaining.is_some() {
            state.limit_events += 1;
        }
        state.exhausted = true;
        work.set(state);
    });
}

/// Share a total budget across the entire reference collection. Nested callers
/// share the active file budget instead of replenishing it. No source/type data
/// survives the synchronous call, a thread switch, or an unwind.
pub(crate) struct FileInferenceBudget {
    owner: bool,
}

pub(crate) struct InferenceWorkReport {
    pub(crate) charged_work: usize,
    pub(crate) scope_quota: usize,
    pub(crate) limit_events: usize,
}

impl FileInferenceBudget {
    pub(crate) fn report(&self) -> Option<InferenceWorkReport> {
        self.owner.then(|| {
            WORK.with(|work| {
                let state = work.get();
                InferenceWorkReport {
                    charged_work: MAX_FILE_INFERENCE_WORK
                        - state.file_remaining.unwrap_or(MAX_FILE_INFERENCE_WORK),
                    scope_quota: state.scope_quota,
                    limit_events: state.limit_events,
                }
            })
        })
    }
    #[cfg(test)]
    pub(crate) fn enter() -> Self {
        Self::with_scopes(1)
    }

    pub(crate) fn for_tree(root: tree_sitter::Node) -> Self {
        let mut scopes = 1usize;
        let mut cursor = root.walk();
        loop {
            if crate::variable_scope::is_callable_scope(cursor.node()) {
                scopes = scopes.saturating_add(1);
            }
            if cursor.goto_first_child() {
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    return Self::with_scopes(scopes);
                }
            }
        }
    }

    fn with_scopes(scopes: usize) -> Self {
        let owner = WORK.with(|work| {
            let mut state = work.get();
            if state.file_remaining.is_some() {
                return false;
            }
            state.file_remaining = Some(MAX_FILE_INFERENCE_WORK);
            state.scope_quota = MAX_FILE_INFERENCE_WORK / scopes.max(1);
            state.limit_events = 0;
            state.scope_remaining = Some(state.scope_quota);
            work.set(state);
            true
        });
        Self { owner }
    }
}

impl Drop for FileInferenceBudget {
    fn drop(&mut self) {
        if self.owner {
            WORK.with(|work| {
                let mut state = work.get();
                state.file_remaining = None;
                state.scope_remaining = None;
                state.scope_quota = 0;
                state.limit_events = 0;
                work.set(state);
            });
        }
    }
}

/// Reserve each lexical callable's share before inference begins. All scopes
/// still debit the same hard file cap; a costly sibling cannot spend the next
/// callable's allocation. Nested scopes restore their parent's remaining share.
pub(crate) struct CallableInferenceBudget {
    previous: Option<usize>,
    active: bool,
}
impl CallableInferenceBudget {
    pub(crate) fn enter() -> Self {
        WORK.with(|work| {
            let mut state = work.get();
            let previous = state.scope_remaining;
            let active = state.file_remaining.is_some();
            if active {
                state.scope_remaining = Some(state.scope_quota);
                work.set(state);
            }
            Self { previous, active }
        })
    }
}
impl Drop for CallableInferenceBudget {
    fn drop(&mut self) {
        if self.active {
            WORK.with(|work| {
                let mut state = work.get();
                state.scope_remaining = self.previous;
                work.set(state);
            });
        }
    }
}

#[cfg(test)]
pub(super) fn remaining() -> usize {
    WORK.with(|work| work.get().file_remaining.unwrap_or(work.get().remaining))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::within_type_resolution_budget;

    #[test]
    fn coverage_report_counts_limits_and_resets_for_another_file() {
        let file = FileInferenceBudget::with_scopes(2);
        let _ = within_type_resolution_budget::<()>(|| {
            while consume() {}
            None
        });
        let report = file.report().unwrap();
        assert!(report.limit_events > 0);
        assert!(report.charged_work <= MAX_FILE_INFERENCE_WORK);
        assert_eq!(report.scope_quota, MAX_FILE_INFERENCE_WORK / 2);
        drop(file);
        let next = FileInferenceBudget::enter();
        let report = next.report().unwrap();
        assert_eq!(report.limit_events, 0);
        assert_eq!(report.charged_work, 0);
    }

    #[test]
    fn sibling_scope_has_reserved_work_after_a_costly_callable() {
        let _file = FileInferenceBudget::with_scopes(3);
        {
            let _scope = CallableInferenceBudget::enter();
            while WORK.with(|work| work.get().scope_remaining.unwrap()) > 0 {
                let _ = within_type_resolution_budget::<()>(|| {
                    while consume() {}
                    None
                });
            }
            assert!(remaining() > 0);
            assert_eq!(within_type_resolution_budget(|| Some(1)), None);
        }
        let _healthy = CallableInferenceBudget::enter();
        assert_eq!(within_type_resolution_budget(|| Some(2)), Some(2));
    }

    #[test]
    fn callable_reservations_cannot_replenish_the_hard_file_cap() {
        let _file = FileInferenceBudget::with_scopes(3);
        for _ in 0..64 {
            let _scope = CallableInferenceBudget::enter();
            let _ = within_type_resolution_budget::<()>(|| {
                while consume() {}
                None
            });
        }
        assert_eq!(remaining(), 0);
        assert_eq!(within_type_resolution_budget(|| Some(1)), None);
    }

    #[test]
    fn callable_unwind_restores_parent_share_without_restoring_spent_file_work() {
        let _file = FileInferenceBudget::with_scopes(3);
        let before = WORK.with(Cell::get);
        assert!(std::panic::catch_unwind(|| {
            let _scope = CallableInferenceBudget::enter();
            within_type_resolution_budget::<()>(|| {
                assert!(consume());
                panic!("scope unwind");
            });
        })
        .is_err());
        let after = WORK.with(Cell::get);
        assert_eq!(after.scope_remaining, before.scope_remaining);
        assert!(after.file_remaining < before.file_remaining);
        assert_eq!(within_type_resolution_budget(|| Some(3)), Some(3));
    }

    #[test]
    fn canonical_callable_inventory_reserves_each_callable_once() {
        let mut parser = crate::parser::FileParser::new();
        parser.parse_full("<?php class C{function method(){}} function outer(){$closure=function(){};$arrow=fn()=>1;function nested(){}}");
        let _file = FileInferenceBudget::for_tree(parser.tree().unwrap().root_node());
        assert_eq!(
            WORK.with(|work| work.get().scope_quota),
            MAX_FILE_INFERENCE_WORK / 6
        );
    }

    #[test]
    fn cst_scan_exhaustion_is_sticky_and_unwind_resets_its_depth() {
        fn nested(depth: usize) -> Option<usize> {
            crate::resolve::within_type_scan_budget(|| {
                if depth > 0 {
                    let _ = nested(depth - 1);
                }
                Some(1)
            })
        }
        assert_eq!(nested(crate::resolve::MAX_OBJECT_TYPE_RESOLVE_DEPTH), None);
        assert!(
            std::panic::catch_unwind(|| crate::resolve::within_type_scan_budget::<()>(|| panic!(
                "scan unwind"
            )))
            .is_err()
        );
        assert_eq!(crate::resolve::CST_TYPE_SCAN_DEPTH.with(Cell::get), 0);
        assert_eq!(crate::resolve::within_type_scan_budget(|| Some(4)), Some(4));
    }

    #[test]
    fn depth_exhaustion_cannot_publish_a_partial_exact_result() {
        fn nested(depth: usize) -> Option<&'static str> {
            within_type_resolution_budget(|| {
                if depth > 0 {
                    let _ = nested(depth - 1);
                }
                Some("earlier exact result")
            })
        }
        assert_eq!(nested(crate::resolve::MAX_OBJECT_TYPE_RESOLVE_DEPTH), None);
        assert_eq!(within_type_resolution_budget(|| Some(3)), Some(3));
    }

    #[test]
    fn exhaustion_is_sticky_and_a_new_request_gets_its_own_budget() {
        assert_eq!(
            within_type_resolution_budget(|| {
                while consume() {}
                assert!(!consume());
                completed(Some("earlier exact result"))
            }),
            None
        );
        assert_eq!(
            within_type_resolution_budget(|| Some("next request")),
            Some("next request")
        );
    }

    #[test]
    fn file_budget_is_shared_by_roots_and_nested_collectors() {
        let file = FileInferenceBudget::enter();
        for _ in 0..32 {
            let _nested = FileInferenceBudget::enter();
            let _ = within_type_resolution_budget(|| {
                while consume() {}
                Some(())
            });
        }
        assert_eq!(remaining(), 0);
        assert_eq!(within_type_resolution_budget(|| Some("stale")), None);
        drop(file);
        assert_eq!(
            within_type_resolution_budget(|| Some("another file")),
            Some("another file")
        );
    }

    #[test]
    fn unwind_releases_file_budget_and_recursion_depth() {
        assert!(std::panic::catch_unwind(|| {
            let _file = FileInferenceBudget::enter();
            within_type_resolution_budget::<()>(|| {
                while consume() {}
                panic!("test unwind");
            });
        })
        .is_err());
        assert_eq!(within_type_resolution_budget(|| Some(7)), Some(7));
    }

    #[test]
    fn simultaneous_threads_cannot_consume_each_others_budget() {
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _file = FileInferenceBudget::enter();
                within_type_resolution_budget::<()>(|| {
                    while consume() {}
                    barrier.wait();
                    barrier.wait();
                    None
                });
            });
            barrier.wait();
            assert_eq!(within_type_resolution_budget(|| Some(9)), Some(9));
            barrier.wait();
        });
    }
}
