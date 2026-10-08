use super::*;
use futures::task::{waker, ArcWake};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll};

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl ArcWake for WakeCounter {
    fn wake_by_ref(this: &Arc<Self>) {
        this.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn poll_once<F: Future>(future: Pin<&mut F>, counter: &Arc<WakeCounter>) -> Poll<F::Output> {
    let waker = waker(counter.clone());
    future.poll(&mut Context::from_waker(&waker))
}

#[test]
fn cancellation_between_flag_check_and_await_is_not_lost() {
    let token = OperationCancellationToken::new();
    let counter = Arc::new(WakeCounter::default());
    let mut armed = true;
    let mut wait = Box::pin(token.cancelled_with_after_check(|| {
        if armed {
            armed = false;
            token.cancel();
        }
    }));
    assert!(
        poll_once(wait.as_mut(), &counter).is_ready(),
        "cancel after the false flag read was lost before the first Notified poll"
    );
    assert!(token.is_cancelled());
}

#[test]
fn cross_thread_cancellation_between_flag_check_and_await_is_not_lost() {
    let token = OperationCancellationToken::new();
    let canceller = token.clone();
    let counter = Arc::new(WakeCounter::default());
    let (checked_tx, checked_rx) = std::sync::mpsc::sync_channel(0);
    let (cancelled_tx, cancelled_rx) = std::sync::mpsc::sync_channel(0);
    let actor = std::thread::spawn(move || {
        checked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        canceller.cancel();
        cancelled_tx.send(()).unwrap();
    });
    let mut armed = true;
    let mut wait = Box::pin(token.cancelled_with_after_check(|| {
        if armed {
            armed = false;
            checked_tx.send(()).unwrap();
            cancelled_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
    }));
    let result = poll_once(wait.as_mut(), &counter);
    actor.join().unwrap();
    assert!(
        result.is_ready(),
        "cross-thread cancellation was lost at the registration boundary"
    );
}

#[test]
fn cancellation_before_wait_creation_is_immediately_ready() {
    let token = OperationCancellationToken::new();
    token.cancel();
    let mut wait = Box::pin(token.cancelled());
    assert!(poll_once(wait.as_mut(), &Arc::new(WakeCounter::default())).is_ready());
}

#[test]
fn cancellation_before_first_future_poll_is_immediately_ready() {
    let token = OperationCancellationToken::new();
    let mut wait = Box::pin(token.cancelled());
    token.clone().cancel();
    assert!(poll_once(wait.as_mut(), &Arc::new(WakeCounter::default())).is_ready());
}

#[test]
fn suspended_waiter_is_woken_by_cancellation_through_a_clone() {
    let token = OperationCancellationToken::new();
    let counter = Arc::new(WakeCounter::default());
    let mut wait = Box::pin(token.cancelled());
    assert!(poll_once(wait.as_mut(), &counter).is_pending());
    assert_eq!(counter.0.load(Ordering::SeqCst), 0);
    token.clone().cancel();
    assert!(
        counter.0.load(Ordering::SeqCst) > 0,
        "a real task waker was not notified"
    );
    assert!(poll_once(wait.as_mut(), &counter).is_ready());
}

#[test]
fn a_spurious_notification_rearms_the_waiter_without_cancelling_it() {
    let token = OperationCancellationToken::new();
    let counter = Arc::new(WakeCounter::default());
    let mut wait = Box::pin(token.cancelled());
    assert!(poll_once(wait.as_mut(), &counter).is_pending());
    token.notify.notify_waiters();
    assert!(!token.is_cancelled());
    assert!(poll_once(wait.as_mut(), &counter).is_pending());
    let previous_wakes = counter.0.load(Ordering::SeqCst);
    token.cancel();
    assert!(counter.0.load(Ordering::SeqCst) > previous_wakes);
    assert!(poll_once(wait.as_mut(), &counter).is_ready());
}

#[test]
fn cancellation_at_the_rearmed_flag_check_is_not_lost() {
    let token = OperationCancellationToken::new();
    let counter = Arc::new(WakeCounter::default());
    let mut checks = 0;
    let mut wait = Box::pin(token.cancelled_with_after_check(|| {
        checks += 1;
        if checks == 2 {
            token.cancel();
        }
    }));
    assert!(poll_once(wait.as_mut(), &counter).is_pending());
    token.notify.notify_waiters();
    assert!(
        poll_once(wait.as_mut(), &counter).is_ready(),
        "a later loop iteration lost cancellation after its flag read"
    );
}

#[test]
fn every_registered_waiter_receives_the_same_cancellation() {
    let token = OperationCancellationToken::new();
    let counters = (0..32)
        .map(|_| Arc::new(WakeCounter::default()))
        .collect::<Vec<_>>();
    let mut waits = (0..counters.len())
        .map(|_| Box::pin(token.cancelled()))
        .collect::<Vec<_>>();
    for (wait, counter) in waits.iter_mut().zip(&counters) {
        assert!(poll_once(wait.as_mut(), counter).is_pending());
    }
    token.cancel();
    for (wait, counter) in waits.iter_mut().zip(&counters) {
        assert!(
            counter.0.load(Ordering::SeqCst) > 0,
            "a waiter missed the broadcast"
        );
        assert!(poll_once(wait.as_mut(), counter).is_ready());
    }
}

#[test]
fn dropping_one_waiter_does_not_cancel_or_disconnect_the_others() {
    let token = OperationCancellationToken::new();
    let counter = Arc::new(WakeCounter::default());
    let mut dropped = Box::pin(token.cancelled());
    let mut survivor = Box::pin(token.cancelled());
    assert!(poll_once(dropped.as_mut(), &counter).is_pending());
    assert!(poll_once(survivor.as_mut(), &counter).is_pending());
    drop(dropped);
    assert!(!token.is_cancelled());
    assert!(poll_once(survivor.as_mut(), &counter).is_pending());
    token.cancel();
    assert!(poll_once(survivor.as_mut(), &counter).is_ready());
}

#[test]
fn repeated_cancel_is_monotonic_and_future_waiters_are_ready() {
    let token = OperationCancellationToken::new();
    for _ in 0..3 {
        token.cancel();
        assert!(token.is_cancelled());
        let mut wait = Box::pin(token.cancelled());
        assert!(poll_once(wait.as_mut(), &Arc::new(WakeCounter::default())).is_ready());
    }
}

#[test]
fn independent_tokens_neither_share_cancellation_nor_wake_each_other() {
    let first = OperationCancellationToken::new();
    let second = OperationCancellationToken::new();
    assert!(first.is_same(&first.clone()));
    assert!(!first.is_same(&second));
    let first_counter = Arc::new(WakeCounter::default());
    let second_counter = Arc::new(WakeCounter::default());
    let mut first_wait = Box::pin(first.cancelled());
    let mut second_wait = Box::pin(second.cancelled());
    assert!(poll_once(first_wait.as_mut(), &first_counter).is_pending());
    assert!(poll_once(second_wait.as_mut(), &second_counter).is_pending());
    first.cancel();
    assert!(poll_once(first_wait.as_mut(), &first_counter).is_ready());
    assert!(!second.is_cancelled());
    assert_eq!(second_counter.0.load(Ordering::SeqCst), 0);
    assert!(poll_once(second_wait.as_mut(), &second_counter).is_pending());
    second.cancel();
    assert!(poll_once(second_wait.as_mut(), &second_counter).is_ready());
}

#[tokio::test(flavor = "current_thread")]
async fn select_losing_branch_and_waiter_recreation_do_not_lose_cancellation() {
    let token = OperationCancellationToken::new();
    tokio::select! {
        biased;
        _ = token.cancelled() => panic!("uncancelled select branch completed"),
        _ = std::future::ready(()) => {}
    }
    assert!(!token.is_cancelled());
    token.cancel();
    tokio::select! {
        biased;
        _ = token.cancelled() => {}
        _ = std::future::ready(()) => panic!("recreated cancelled branch was pending"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_parallel_registration_and_cancellation_releases_all_waiters() {
    tokio::time::timeout(Duration::from_secs(5), async {
        for _ in 0..128 {
            let token = OperationCancellationToken::new();
            let gate = Arc::new(tokio::sync::Barrier::new(17));
            let mut waits = tokio::task::JoinSet::new();
            for _ in 0..16 {
                let waiter = token.clone();
                let gate = gate.clone();
                waits.spawn(async move {
                    gate.wait().await;
                    waiter.cancelled().await;
                    assert!(waiter.is_cancelled());
                });
            }
            gate.wait().await;
            token.cancel();
            while let Some(result) = waits.join_next().await {
                result.unwrap();
            }
        }
    })
    .await
    .expect("parallel cancellation waiters stalled");
}
