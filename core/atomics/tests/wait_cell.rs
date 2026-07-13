use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use tokio::time::timeout;
use vprogs_core_atomics::WaitCell;

// 1. Predicate already satisfied resolves without any store.
#[tokio::test]
async fn ready_when_predicate_already_true() {
    let cell = WaitCell::new(0);
    timeout(Duration::from_secs(5), cell.wait_until(|v| v == 0))
        .await
        .expect("already-true predicate must resolve immediately");
}

// 2. A store wakes a parked waiter.
#[tokio::test]
async fn wakes_on_store() {
    let cell = Arc::new(WaitCell::new(0));
    let c = cell.clone();
    let waiter = tokio::spawn(async move { c.wait_until(|v| v == 2).await });
    tokio::task::yield_now().await;
    cell.store(2);
    timeout(Duration::from_secs(5), waiter)
        .await
        .expect("waiter stalled")
        .expect("waiter task panicked");
}

// 3. Window A: a store landing after the future is created but before its first poll is observed
//    through the eagerly-registered Notified. A non-satisfying store keeps the predicate false, so
//    the value load cannot short-circuit; only the pre-registered notification drives the wake,
//    forcing a re-arm and a second predicate check before the future parks. A satisfying store then
//    resolves it.
#[test]
fn store_before_first_poll_is_observed_via_notify() {
    let cell = WaitCell::new(0);
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);

    let pred_calls = std::cell::Cell::new(0usize);
    let mut fut = Box::pin(cell.wait_until(|v| {
        pred_calls.set(pred_calls.get() + 1);
        v == 2
    }));

    // Store a non-satisfying value before the first poll: it fires notify_waiters but leaves the
    // predicate false, so any progress must come from the notification registered at creation.
    cell.store(1);
    assert!(fut.as_mut().poll(&mut cx).is_pending());
    assert_eq!(
        pred_calls.get(),
        2,
        "pre-poll notify must wake the Notified: check, wake, re-arm, re-check"
    );

    cell.store(2);
    assert!(fut.as_mut().poll(&mut cx).is_ready());
}

// 4. Window B: store lands between the predicate's load and the notified registration.
#[tokio::test]
async fn wakes_when_store_between_load_and_register() {
    let cell = WaitCell::new(0);
    let fired = std::cell::Cell::new(false);
    let fut = cell.wait_until(|v| {
        if !fired.get() {
            fired.set(true);
            cell.store(2);
        }
        v == 2
    });
    timeout(Duration::from_secs(5), fut)
        .await
        .expect("stalled: lost wakeup in the load->register window");
}

// 5. Every waiter wakes at its own target. A store for one target spuriously wakes waiters for the
//    other target, which re-check and re-park. The intermediate value 1 is held until its waiters
//    drain before advancing to 2: this cell is level-triggered (a waiter observes the current
//    value, not a transient past one), so a target that is overwritten before its waiters run could
//    never be observed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_waiters_wake_at_their_target() {
    let cell = Arc::new(WaitCell::new(0));

    let mid_waiters: Vec<_> = (0..8)
        .map(|_| {
            let c = cell.clone();
            tokio::spawn(async move { c.wait_until(|v| v == 1).await })
        })
        .collect();
    let hi_waiters: Vec<_> = (0..8)
        .map(|_| {
            let c = cell.clone();
            tokio::spawn(async move { c.wait_until(|v| v == 2).await })
        })
        .collect();

    // Store 1 and drain its waiters while it still holds; the 2-waiters are spuriously woken by
    // this store and must re-park rather than resolve.
    cell.store(1);
    for h in mid_waiters {
        timeout(Duration::from_secs(5), h).await.expect("mid waiter stalled").expect("panicked");
    }

    // Advance to the final target; the 2-waiters resolve now.
    cell.store(2);
    for h in hi_waiters {
        timeout(Duration::from_secs(5), h).await.expect("hi waiter stalled").expect("panicked");
    }
}

// 6. Blocking twin wakes from a cross-thread store (no tokio runtime).
#[test]
fn blocking_twin_wakes() {
    use std::thread;

    let cell = Arc::new(WaitCell::new(0));
    let c = cell.clone();
    let waiter = thread::spawn(move || c.wait_until_blocking(|v| v == 2));
    thread::sleep(Duration::from_millis(100));
    cell.store(2);
    waiter.join().expect("blocking waiter panicked");
}

// 7. Stress: many waiters, repeated rounds, none may hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stress_all_waiters_wake() {
    for _ in 0..2000 {
        let cell = Arc::new(WaitCell::new(0));
        let mut handles = Vec::new();
        for _ in 0..64 {
            let c = cell.clone();
            handles.push(tokio::spawn(async move { c.wait_until(|v| v == 2).await }));
        }
        cell.store(1);
        cell.store(2);
        for h in handles {
            timeout(Duration::from_secs(5), h).await.expect("waiter stalled").expect("panicked");
        }
    }
}

// 8. Dropping a pending waiter is safe and leaves the cell usable.
#[tokio::test]
async fn dropping_pending_waiter_is_safe() {
    let cell = WaitCell::new(0);
    {
        let fut = cell.wait_until(|v| v == 2);
        futures::pin_mut!(fut);
        assert!(matches!(futures::poll!(fut.as_mut()), Poll::Pending));
        // fut drops at the end of this block while still pending.
    }
    cell.store(2);
}

/// Waker that counts invocations, for driving `WaitUntil` by hand without a runtime.
struct CountingWaker(AtomicUsize);

impl futures::task::ArcWake for CountingWaker {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        arc_self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl CountingWaker {
    fn wakes(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

// 9. Manual poll: a store fires the waker a parked waiter registered, and the re-poll resolves. No
//    runtime and no timing; every step is sequenced by hand, so a missed wake fails an assert
//    deterministically instead of relying on a scheduler to expose it as a hang.
#[test]
fn store_fires_parked_waiters_waker() {
    let cell = WaitCell::new(0);
    let wakes = Arc::new(CountingWaker(AtomicUsize::new(0)));
    let waker = futures::task::waker(wakes.clone());
    let mut cx = Context::from_waker(&waker);

    let mut fut = Box::pin(cell.wait_until(|v| v == 2));
    assert!(fut.as_mut().poll(&mut cx).is_pending());
    assert_eq!(wakes.wakes(), 0, "no store yet, nothing may wake");

    cell.store(2);
    assert_eq!(wakes.wakes(), 1, "store must fire the parked waiter's waker");
    assert!(fut.as_mut().poll(&mut cx).is_ready());
}

// 10. Manual poll of the re-arm path: a store for the wrong target fires the waker, the re-poll
//     re-arms and parks again, and the right-target store fires the waker a second time, proving
//     the re-armed Notified was re-registered with the task's waker rather than dropped on the
//     floor. The exact predicate-call count pins the poll loop: one check on the first poll, two on
//     the spurious wake (before and after re-arming), one on the final poll.
#[test]
fn wrong_target_wake_reparks_and_rewakes() {
    let cell = WaitCell::new(0);
    let wakes = Arc::new(CountingWaker(AtomicUsize::new(0)));
    let waker = futures::task::waker(wakes.clone());
    let mut cx = Context::from_waker(&waker);

    let mut pred_calls = 0usize;
    let mut fut = Box::pin(cell.wait_until(|v| {
        pred_calls += 1;
        v == 2
    }));
    assert!(fut.as_mut().poll(&mut cx).is_pending());

    cell.store(1);
    assert_eq!(wakes.wakes(), 1, "a store for another target must still wake the waiter");
    assert!(fut.as_mut().poll(&mut cx).is_pending(), "unsatisfied waiter must park again");

    cell.store(2);
    assert_eq!(wakes.wakes(), 2, "the re-armed waiter must have been re-registered");
    assert!(fut.as_mut().poll(&mut cx).is_ready());

    drop(fut);
    assert_eq!(pred_calls, 4);
}

// 11. A waiter that was woken but never re-polled is dropped in the notified-but-unconsumed state;
//     a sibling waiter woken by the same store must still resolve, and the cell stays usable.
#[test]
fn dropping_woken_unpolled_waiter_leaves_sibling_intact() {
    let cell = WaitCell::new(0);
    let wakes1 = Arc::new(CountingWaker(AtomicUsize::new(0)));
    let waker1 = futures::task::waker(wakes1.clone());
    let mut cx1 = Context::from_waker(&waker1);
    let wakes2 = Arc::new(CountingWaker(AtomicUsize::new(0)));
    let waker2 = futures::task::waker(wakes2.clone());
    let mut cx2 = Context::from_waker(&waker2);

    let mut fut1 = Box::pin(cell.wait_until(|v| v == 2));
    let mut fut2 = Box::pin(cell.wait_until(|v| v == 2));
    assert!(fut1.as_mut().poll(&mut cx1).is_pending());
    assert!(fut2.as_mut().poll(&mut cx2).is_pending());

    cell.store(2);
    assert_eq!(wakes1.wakes(), 1);
    assert_eq!(wakes2.wakes(), 1);

    drop(fut1);
    assert!(fut2.as_mut().poll(&mut cx2).is_ready());
    cell.store(0);
}

// 12. Blocking twin returns immediately when the predicate already holds (no runtime, no store).
#[test]
fn blocking_twin_ready_without_store() {
    let cell = WaitCell::new(1);
    cell.wait_until_blocking(|v| v == 1);
}
