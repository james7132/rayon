use crate::latch::{Latch, LockLatch, SpinLatch};
use crate::registry::WorkerThread;

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake};

/// Wait for the result from a `Future`, allowing other work to continue in the
/// threadpool in the meantime.
///
/// FIXME: this behaves poorly with nesting, which work-stealing exacerbates.
pub fn block_on<F: IntoFuture>(future: F) -> F::Output {
    // Pin the future so it can be polled, shadowing the binding to ensure the
    // owned future is otherwise inaccessible.
    let mut future = future.into_future();
    let mut future = unsafe { Pin::new_unchecked(&mut future) };

    if let Some(thread) = unsafe { WorkerThread::current().as_ref() } {
        let waker = LatchWaker::new(SpinLatch::new_static(thread));
        let cx_waker = Arc::clone(&waker).into();
        let mut cx = Context::from_waker(&cx_waker);
        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(res) => return res,
                Poll::Pending => unsafe {
                    thread.wait_until(&waker.latch);
                    waker.latch.reset();
                },
            }
        }
    } else {
        let waker = LatchWaker::new(LockLatch::new());
        let cx_waker = Arc::clone(&waker).into();
        let mut cx = Context::from_waker(&cx_waker);
        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(res) => return res,
                Poll::Pending => {
                    waker.latch.wait_and_reset();
                }
            }
        }
    }
}

struct LatchWaker<L> {
    latch: L,
}

impl<L> LatchWaker<L> {
    fn new(latch: L) -> Arc<Self> {
        Arc::new(Self { latch })
    }
}

impl<L: Latch> Wake for LatchWaker<L> {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        unsafe { L::set(&self.latch) };
    }
}

#[test]
fn block_on_channel() {
    const COUNT: usize = 100;

    let pool = crate::ThreadPoolBuilder::new()
        .num_threads(3)
        .build()
        .unwrap();

    let (main_tx, main_rx) = async_channel::unbounded();
    let (pool_tx, pool_rx) = async_channel::unbounded();
    for _ in 0..COUNT {
        let pool_rx = pool_rx.clone();
        let main_tx = main_tx.clone();
        pool.spawn(move || {
            block_on(async {
                assert_eq!(pool_rx.recv().await, Ok(true));
                assert_eq!(main_tx.send(false).await, Ok(()));
            })
        });
    }
    drop((pool_rx, main_tx));

    block_on(async {
        for _ in 0..COUNT {
            assert_eq!(pool_tx.send(true).await, Ok(()));
        }
        for _ in 0..COUNT {
            assert_eq!(main_rx.recv().await, Ok(false));
        }
        assert!(main_rx.recv().await.is_err());
    });
}