//! Synchronous inference work limits. Depth alone does not bound branching
//! fallback inference or repeated scans of preceding statements.

use std::cell::Cell;

pub(super) const MAX_INFERENCE_WORK: usize = 8_192;
const MAX_FILE_INFERENCE_WORK: usize = 131_072;

#[derive(Clone, Copy)]
struct Work {
    remaining: usize,
    file_remaining: Option<usize>,
    exhausted: bool,
}

thread_local! {
    static WORK: Cell<Work> = const { Cell::new(Work {
        remaining: MAX_INFERENCE_WORK,
        file_remaining: None,
        exhausted: false,
    }) };
}

pub(super) fn begin() {
    WORK.with(|work| {
        let mut state = work.get();
        state.remaining = MAX_INFERENCE_WORK;
        state.exhausted = state.file_remaining == Some(0);
        work.set(state);
    });
}

pub(super) fn consume() -> bool {
    WORK.with(|work| {
        let mut state = work.get();
        if state.exhausted || state.remaining == 0 || state.file_remaining == Some(0) {
            state.exhausted = true;
            work.set(state);
            return false;
        }
        state.remaining -= 1;
        if let Some(remaining) = &mut state.file_remaining {
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

impl FileInferenceBudget {
    pub(crate) fn enter() -> Self {
        let owner = WORK.with(|work| {
            let mut state = work.get();
            if state.file_remaining.is_some() {
                return false;
            }
            state.file_remaining = Some(MAX_FILE_INFERENCE_WORK);
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
