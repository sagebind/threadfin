use std::{
    future::Future,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    pin::Pin,
    ptr,
    sync::Arc,
    task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
    thread,
    time::Duration,
};

use atomic_waker::AtomicWaker;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, TryRecvError};

/// A type of future representing the result of a background computation in a
/// thread pool.
///
/// Tasks implement [`Future`], so you can `.await` their completion
/// asynchronously. Or, you can wait for their completion synchronously using
/// the various methods provided.
///
/// If a task is dropped before it can be executed then its execution will be
/// canceled. Canceling a synchronous closure after it has already started will
/// have no effect.
pub struct Task<T: Send> {
    receiver: Receiver<thread::Result<T>>,
    waker: Arc<AtomicWaker>,
}

impl<T: Send> Task<T> {
    /// Create a new task from a closure.
    pub(crate) fn from_closure<F>(closure: F) -> (Self, Runner)
    where
        F: FnOnce() -> T + Send + 'static,
        T: 'static,
    {
        let (tx, rx) = bounded(1);

        let task = Task {
            receiver: rx,
            waker: Arc::new(AtomicWaker::new()),
        };

        let waker = task.waker.clone();
        let mut closure = Some(closure);

        let runner = Runner {
            poll: Box::new(move |_cx| {
                let closure = closure.take().unwrap();
                let result = catch_unwind(AssertUnwindSafe(closure));

                if tx.send(result).is_err() {
                    waker.wake();
                    log::trace!("task canceled before it could run, ignoring");
                }

                true
            }),
            waker: empty_waker(),
            might_yield: false,
            complete: false,
        };

        (task, runner)
    }

    /// Create a new asynchronous task from a future.
    pub(crate) fn from_future<F>(future: F) -> (Self, Runner)
    where
        F: Future<Output = T> + Send + 'static,
        T: 'static,
    {
        let mut future = Box::pin(future);
        let (tx, rx) = bounded(1);

        let task = Task {
            receiver: rx,
            waker: Arc::new(AtomicWaker::new()),
        };

        let waker = task.waker.clone();

        let runner = Runner {
            poll: Box::new(move |cx| {
                let future = future.as_mut();

                let result = match catch_unwind(AssertUnwindSafe(|| future.poll(cx))) {
                    Ok(Poll::Pending) => return false,
                    Ok(Poll::Ready(value)) => Ok(value),
                    Err(e) => Err(e),
                };

                if tx.send(result).is_err() {
                    waker.wake();
                    log::trace!("task canceled before it could run, ignoring");
                }

                true
            }),
            waker: empty_waker(),
            might_yield: true,
            complete: false,
        };

        (task, runner)
    }

    /// Check if the task is done yet.
    pub fn is_done(&self) -> bool {
        !self.receiver.is_empty()
    }

    pub fn try_get(&mut self) -> Option<thread::Result<T>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(e) => Some(Err(Box::new(e))),
        }
    }

    /// Block the current thread until the task completes.
    ///
    /// # Panics
    ///
    /// If the closure the task was created from panics, the panic will
    /// propagate to this call.
    pub fn get(self) -> T {
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
    /// If the closure the task was created from panics, the panic will
    /// propagate to this call.
    pub fn get_timeout(self, timeout: Duration) -> Result<T, Self> {
        match self.receiver.recv_timeout(timeout) {
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
pub(crate) struct Runner {
    poll: Box<dyn FnMut(&mut Context) -> bool + Send>,
    waker: Waker,

    /// Whether this task might yield. This can be used for optimizations if you
    /// know for certain a waker will never be used.
    might_yield: bool,

    /// Indicates when the task is complete.
    complete: bool,
}

impl Runner {
    pub(crate) fn might_yield(&self) -> bool {
        self.might_yield
    }

    /// Set the waker to use with this task.
    pub(crate) fn set_waker(&mut self, waker: Waker) {
        self.waker = waker;
    }

    /// Poll the underlying future, returning `true` when the future completes.
    ///
    /// If `false` is returned, you should call `poll` again once the waker
    /// associated with the runner is called.
    ///
    /// Calling this function after the first time it returns `true` is a no-op
    /// and will return `true`.
    pub(crate) fn run(&mut self) -> bool {
        if !self.complete {
            let mut cx = Context::from_waker(&self.waker);
            self.complete = (self.poll)(&mut cx);
        }

        self.complete
    }
}

fn empty_waker() -> Waker {
    const RAW_WAKER: RawWaker = RawWaker::new(ptr::null(), &VTABLE);
    const VTABLE: RawWakerVTable = RawWakerVTable::new(|_| RAW_WAKER, |_| {}, |_| {}, |_| {});

    unsafe { Waker::from_raw(RAW_WAKER) }
}
