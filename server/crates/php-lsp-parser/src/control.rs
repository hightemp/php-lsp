//! Cancellation scope for synchronous, read-only analysis on a worker poll.
//! Callers must discard cancelled results; mutation/indexing work keeps its own
//! explicit commit/cancellation protocol and does not inherit this scope.
use std::cell::RefCell;
use std::sync::Arc;

pub type CancellationCheck = Arc<dyn Fn() -> bool + Send + Sync>;
thread_local! {static ACTIVE:RefCell<Option<CancellationCheck>>=const {RefCell::new(None)};}
struct Restore(Option<CancellationCheck>);
impl Drop for Restore {
    fn drop(&mut self) {
        ACTIVE.with(|active| *active.borrow_mut() = self.0.take());
    }
}
pub fn is_cancelled() -> bool {
    ACTIVE.with(|active| active.borrow().as_ref().is_some_and(|check| check()))
}
pub fn run_cancellable<T>(check: CancellationCheck, op: impl FnOnce() -> T) -> Option<T> {
    let _restore = Restore(ACTIVE.with(|active| active.replace(Some(check))));
    if is_cancelled() {
        return None;
    }
    let result = op();
    (!is_cancelled()).then_some(result)
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
