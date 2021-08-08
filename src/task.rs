//! Implementation of a task, as well as underlying primitives used to drive
//! their execution.

use std::{future::Future, panic::{catch_unwind, resume_unwind, AssertUnwindSafe}, pin::Pin, ptr, sync::Arc, task::{Context, Poll, RawWaker, RawWakerVTable, Waker}, thread, time::{Duration, Instant}};

use atomic_waker::AtomicWaker;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, TryRecvError};

/// A type of future representing the result of a background computation in a
/// thread pool.
///
/// Tasks implement [`Future`], so you can `.await` their completion
/// asynchronously. Or, you can wait for their completion synchronously using
/// the various `join*` methods provided.
///
/// If a task is dropped before it can be executed then its execution will be
/// canceled. Canceling a synchronous closure after it has already started will
/// have no effect.
pub struct Task<T: Send> {
    receiver: Receiver<thread::Result<T>>,
    waker: AssertUnwindSafe<Arc<AtomicWaker>>,
}

impl<T: Send> Task<T> {
    /// Create a new task from a closure.
    pub(crate) fn from_closure<F>(closure: F) -> (Self, Coroutine)
    where
        F: FnOnce() -> T + Send + 'static,
        T: 'static,
    {
        let mut closure = Some(closure);

        Self::new(false, move |_cx| {
            let closure = closure.take().unwrap();
            Some(catch_unwind(AssertUnwindSafe(closure)))
        })
    }

    /// Create a new asynchronous task from a future.
    pub(crate) fn from_future<F>(future: F) -> (Self, Coroutine)
    where
        F: Future<Output = T> + Send + 'static,
        T: 'static,
    {
        let mut future = Box::pin(future);

        Self::new(true, move |cx| {
            let future = future.as_mut();

            match catch_unwind(AssertUnwindSafe(|| future.poll(cx))) {
                Ok(Poll::Pending) => None,
                Ok(Poll::Ready(value)) => Some(Ok(value)),
                Err(e) => Some(Err(e)),
            }
        })
    }

    fn new(
        might_yield: bool,
        mut poll: impl FnMut(&mut Context) -> Option<thread::Result<T>> + Send + 'static,
    ) -> (Self, Coroutine)
    where
        T: 'static,
    {
        let (tx, rx) = bounded(1);

        let task = Task {
            receiver: rx,
            waker: AssertUnwindSafe(Arc::new(AtomicWaker::new())),
        };

        let mut tx = Some(tx);

        let coroutine = Coroutine {
            might_yield,
            last_result: RunResult::Yield,
            waker: empty_waker(),
            task_waker: task.waker.clone(),
            poll: Box::new(move |cx| {
                if let Some(result) = poll(cx) {
                    let panicked = result.is_err();

                    if tx.take().unwrap().send(result).is_err() {
                        // task canceled before it could run, do nothing
                    }

                    RunResult::Complete { panicked }
                } else {
                    RunResult::Yield
                }
            }),
        };

        (task, coroutine)
    }

    /// Check if the task is done yet.
    pub fn is_done(&self) -> bool {
        !self.receiver.is_empty()
    }

    /// Block the current thread until the task completes.
    ///
    /// # Panics
    ///
    /// If the underlying task panics, the panic will propagate to this call.
    pub fn join(self) -> T {
        match self.receiver.recv().unwrap() {
            Ok(value) => value,
            Err(e) => resume_unwind(e),
        }
    }

    /// Block the current thread until the task completes or a timeout is
    /// reached.
    ///
    /// # Panics
    ///
    /// If the underlying task panics, the panic will propagate to this call.
    pub fn join_timeout(self, timeout: Duration) -> Result<T, Self> {
        self.join_deadline(Instant::now() + timeout)
    }

    /// Block the current thread until the task completes or a timeout is
    /// reached.
    ///
    /// # Panics
    ///
    /// If the underlying task panics, the panic will propagate to this call.
    pub fn join_deadline(self, deadline: Instant) -> Result<T, Self> {
        match self.receiver.recv_deadline(deadline) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => resume_unwind(e),
            Err(RecvTimeoutError::Timeout) => Err(self),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("task was dropped by thread pool without being completed")
            }
        }
    }
}

impl<T: Send> Future for Task<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.waker.register(cx.waker());

        match self.receiver.try_recv() {
            Ok(Ok(value)) => Poll::Ready(value),
            Ok(Err(e)) => resume_unwind(e),
            Err(TryRecvError::Empty) => Poll::Pending,
            Err(TryRecvError::Disconnected) => {
                panic!("task was dropped by thread pool without being completed")
            }
        }
    }
}

/// The worker side of an allocated task, which provides methods for running the
/// underlying future to completion.
pub(crate) struct Coroutine {
    might_yield: bool,
    last_result: RunResult,
    waker: Waker,
    task_waker: Arc<AtomicWaker>,
    poll: Box<dyn FnMut(&mut Context) -> RunResult + Send>,
}

impl Coroutine {
    /// Determine whether this task might yield. This can be used for
    /// optimizations if you know for certain a waker will never be used.
    pub(crate) fn might_yield(&self) -> bool {
        self.might_yield
    }

    /// Set the waker to use with this task.
    pub(crate) fn set_waker(&mut self, waker: Waker) {
        self.waker = waker;
    }

    /// Run the coroutine until it yields.
    ///
    /// Calling this function after the first time it returns `Complete` is a
    /// no-op and will continue to return `Complete`.
    pub(crate) fn run(&mut self) -> RunResult {
        if self.last_result == RunResult::Yield {
            let mut cx = Context::from_waker(&self.waker);
            self.last_result = (self.poll)(&mut cx);
        }

        self.last_result
    }

    /// Notify any listeners on this task that the task's state has updated.
    ///
    /// You must call this yourself when the task completes. It won't be called
    /// automatically!
    pub(crate) fn notify(&self) {
        self.task_waker.wake();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunResult {
    /// The coroutine has yielded. You should call `run` on the coroutine again
    /// once the waker associated with the coroutine is called.
    Yield,

    /// The coroutine and its associated task has completed. You should call
    /// [`Coroutine::notify`] to wake any consumers of the task to receive the
    /// task result.
    Complete {
        panicked: bool,
    },
}

/// Creates a dummy waker that does nothing.
fn empty_waker() -> Waker {
    const RAW_WAKER: RawWaker = RawWaker::new(ptr::null(), &VTABLE);
    const VTABLE: RawWakerVTable = RawWakerVTable::new(|_| RAW_WAKER, |_| {}, |_| {}, |_| {});

    unsafe { Waker::from_raw(RAW_WAKER) }
}
