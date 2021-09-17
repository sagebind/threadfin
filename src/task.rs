//! Implementation of a task, as well as underlying primitives used to drive
//! their execution.

use std::{
    future::Future,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    pin::Pin,
    ptr,
    sync::Arc,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    thread,
    time::{Duration, Instant},
};

use atomic_waker::AtomicWaker;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TryRecvError};

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
        T: Send + 'static,
    {
        let (sender, receiver) = bounded(1);

        let task = Task {
            receiver,
            waker: AssertUnwindSafe(Arc::new(AtomicWaker::new())),
        };

        let coroutine = Coroutine {
            might_yield: false,
            waker: empty_waker(),
            task_waker: task.waker.clone(),
            poller: Box::new(ClosurePoller {
                closure: Some(closure),
                result: None,
                sender,
            }),
        };

        (task, coroutine)
    }

    /// Create a new asynchronous task from a future.
    pub(crate) fn from_future<F>(future: F) -> (Self, Coroutine)
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (sender, receiver) = bounded(1);

        let task = Task {
            receiver,
            waker: AssertUnwindSafe(Arc::new(AtomicWaker::new())),
        };

        let coroutine = Coroutine {
            might_yield: true,
            waker: empty_waker(),
            task_waker: task.waker.clone(),
            poller: Box::new(FuturePoller {
                future,
                result: None,
                sender,
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
    waker: Waker,
    task_waker: Arc<AtomicWaker>,
    poller: Box<dyn CoroutinePoller>,
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

    /// Run the coroutine until it yields or completes.
    ///
    /// Once this function returns `Complete` it should not be called again. Doing
    /// so may panic, return weird results, or cause other problems.
    pub(crate) fn run(&mut self) -> RunResult {
        let mut cx = Context::from_waker(&self.waker);
        self.poller.run(&mut cx)
    }

    /// Complete the task with the final value produced by this coroutine and
    /// notify any listeners on this task that the task's state has updated.
    ///
    /// Must not be called unless `run` has returned `Complete`. This method may
    /// panic or cause other strange behavior otherwise.
    ///
    /// You must call this yourself when the task completes. It won't be called
    /// automatically!
    pub(crate) fn complete(mut self) {
        self.poller.complete();
        self.task_waker.wake();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RunResult {
    /// The coroutine has yielded. You should call `run` on the coroutine again
    /// once the waker associated with the coroutine is called.
    Yield,

    /// The coroutine and its associated task has completed. You should call
    /// [`Coroutine::complete`] to wake any consumers of the task to receive the
    /// task result.
    Complete { panicked: bool },
}

/// Creates a dummy waker that does nothing.
fn empty_waker() -> Waker {
    const RAW_WAKER: RawWaker = RawWaker::new(ptr::null(), &VTABLE);
    const VTABLE: RawWakerVTable = RawWakerVTable::new(|_| RAW_WAKER, |_| {}, |_| {}, |_| {});

    unsafe { Waker::from_raw(RAW_WAKER) }
}

/// Inner implementation of a coroutine. This trait is used to erase the return
/// value from the coroutine type as well as to abstract over futures and
/// synchronous closures. Bundling all the required operations into this trait
/// also allows us to minimize the number of heap allocations per task.
trait CoroutinePoller: Send {
    fn run(&mut self, cx: &mut Context) -> RunResult;

    fn complete(&mut self);
}

struct ClosurePoller<F, T> {
    closure: Option<F>,
    result: Option<thread::Result<T>>,
    sender: Sender<thread::Result<T>>,
}

impl<F, T> CoroutinePoller for ClosurePoller<F, T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send,
{
    fn run(&mut self, _cx: &mut Context) -> RunResult {
        let closure = self
            .closure
            .take()
            .expect("closure already ran to completion");
        let result = catch_unwind(AssertUnwindSafe(closure));
        let panicked = result.is_err();

        self.result = Some(result);

        RunResult::Complete { panicked }
    }

    fn complete(&mut self) {
        if let Some(result) = self.result.take() {
            if self.sender.send(result).is_err() {
                // task canceled before it could run, do nothing
            }
        }
    }
}

struct FuturePoller<F, T> {
    future: F,
    result: Option<thread::Result<T>>,
    sender: Sender<thread::Result<T>>,
}

impl<F, T> CoroutinePoller for FuturePoller<F, T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send,
{
    fn run(&mut self, cx: &mut Context) -> RunResult {
        // Safety: This struct is only ever used inside a box, so we know that
        // neither self nor this future will move.
        let future = unsafe { Pin::new_unchecked(&mut self.future) };

        match catch_unwind(AssertUnwindSafe(|| future.poll(cx))) {
            Ok(Poll::Pending) => RunResult::Yield,
            Ok(Poll::Ready(value)) => {
                self.result = Some(Ok(value));

                RunResult::Complete { panicked: false }
            }
            Err(e) => {
                self.result = Some(Err(e));

                RunResult::Complete { panicked: true }
            }
        }
    }

    fn complete(&mut self) {
        if let Some(result) = self.result.take() {
            if self.sender.send(result).is_err() {
                // task canceled before it could run, do nothing
            }
        }
    }
}
