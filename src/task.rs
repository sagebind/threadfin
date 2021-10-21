//! Implementation of a task, as well as underlying primitives used to drive
//! their execution.

use std::{
    any::Any,
    fmt,
    future::Future,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    thread,
    time::{Duration, Instant},
};

/// A type of future representing the result of a background computation in a
/// thread pool. Tasks are returned when submitting a closure or future to a
/// thread pool.
///
/// Tasks implement [`Future`], so you can `.await` their completion
/// asynchronously. Or, you can wait for their completion synchronously using
/// the various [`join`](Task::join) methods provided.
///
/// Dropping a task detaches the running task, but does not cancel it. The task
/// will continue to run on the thread pool until completion, but there will no
/// longer be any way to check its completion or to retrieve its returned value.
///
/// # Examples
///
/// Creating a task:
///
/// ```
/// use threadfin::ThreadPool;
///
/// let pool = ThreadPool::new();
///
/// let task = pool.execute(|| {
///     // do some work
/// });
/// ```
///
/// Blocking on a task:
///
/// ```
/// use threadfin::ThreadPool;
///
/// let pool = ThreadPool::new();
///
/// let task = pool.execute(|| {
///     // some expensive computation
///     2 + 2
/// });
///
/// // do something in the meantime
///
/// // now block on the result
/// let sum = task.join();
/// assert_eq!(sum, 4);
/// ```
///
/// Awaiting a task asynchronously:
///
/// ```
/// # threadfin::common().execute_future(async {
/// use threadfin::ThreadPool;
///
/// let pool = ThreadPool::new();
///
/// let task = pool.execute(|| {
///     // some expensive, synchronous computation
///     2 + 2
/// });
///
/// // do something in the meantime
///
/// // now await on the result
/// let sum = task.await;
/// assert_eq!(sum, 4);
/// # }).join();
/// ```
///
/// Detaching a task:
///
/// ```
/// use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
/// use std::thread::sleep;
/// use std::time::Duration;
/// use threadfin::ThreadPool;
///
/// let pool = Arc::new(ThreadPool::new());
/// let completed = Arc::new(AtomicBool::from(false));
///
/// // Clone the shared values to be used inside the task.
/// let pool_clone = pool.clone();
/// let completed_clone = completed.clone();
///
/// pool.execute(move || {
///     let _inner_task = pool_clone.execute(move || {
///         // Short delay simulating some work.
///         sleep(Duration::from_millis(100));
///
///         // Set as complete.
///         completed_clone.store(true, Ordering::SeqCst);
///     });
///
///     // Inner task is detached, but will still complete.
/// });
///
/// // Give the task some time to complete.
/// sleep(Duration::from_millis(1000));
///
/// // Inner task completed even though it was detached.
/// assert_eq!(completed.load(Ordering::SeqCst), true);
/// ```
pub struct Task<T> {
    inner: Arc<Mutex<Inner<T>>>,
}

struct Inner<T> {
    result: Option<thread::Result<T>>,
    waker: Option<Waker>,
}

impl<T> Task<T> {
    /// Create a new task from a closure.
    pub(crate) fn from_closure<F>(closure: F) -> (Self, Coroutine)
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let task = Self::pending();

        let coroutine = Coroutine {
            might_yield: false,
            waker: crate::wakers::empty_waker(),
            poller: Box::new(ClosurePoller {
                closure: Some(closure),
                result: None,
                task: task.inner.clone(),
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
        let task = Self::pending();

        let coroutine = Coroutine {
            might_yield: true,
            waker: crate::wakers::empty_waker(),
            poller: Box::new(FuturePoller {
                future,
                result: None,
                task: task.inner.clone(),
            }),
        };

        (task, coroutine)
    }

    fn pending() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                result: None,
                waker: None,
            })),
        }
    }

    /// Check if the task is done yet.
    ///
    /// If this method returns true, then calling [`join`](Task::join) will not
    /// block.
    pub fn is_done(&self) -> bool {
        self.inner.lock().unwrap().result.is_some()
    }

    /// Block the current thread until the task completes and return the value
    /// the task produced.
    ///
    /// # Panics
    ///
    /// If the underlying task panics, the panic will propagate to this call.
    pub fn join(self) -> T {
        match self.join_catch() {
            Ok(value) => value,
            Err(e) => resume_unwind(e),
        }
    }

    fn join_catch(self) -> thread::Result<T> {
        let mut inner = self.inner.lock().unwrap();

        if let Some(result) = inner.result.take() {
            result
        } else {
            inner.waker = Some(crate::wakers::current_thread_waker());
            drop(inner);

            loop {
                thread::park();

                if let Some(result) = self.inner.lock().unwrap().result.take() {
                    break result;
                }
            }
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
        match {
            let mut inner = self.inner.lock().unwrap();

            if let Some(result) = inner.result.take() {
                result
            } else {
                inner.waker = Some(crate::wakers::current_thread_waker());
                drop(inner);

                loop {
                    if let Some(timeout) = deadline.checked_duration_since(Instant::now()) {
                        thread::park_timeout(timeout);
                    } else {
                        return Err(self);
                    }

                    if let Some(result) = self.inner.lock().unwrap().result.take() {
                        break result;
                    }
                }
            }
        } {
            Ok(value) => Ok(value),
            Err(e) => resume_unwind(e),
        }
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.inner.lock().unwrap();

        match inner.result.take() {
            Some(Ok(value)) => Poll::Ready(value),
            Some(Err(e)) => resume_unwind(e),
            None => {
                inner.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl<T> fmt::Debug for Task<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task")
            .field("done", &self.is_done())
            .finish()
    }
}

/// The worker side of an allocated task, which provides methods for running the
/// underlying future to completion.
pub(crate) struct Coroutine {
    might_yield: bool,
    waker: Waker,
    poller: Box<dyn CoroutinePoller>,
}

impl Coroutine {
    /// Determine whether this task might yield. This can be used for
    /// optimizations if you know for certain a waker will never be used.
    pub(crate) fn might_yield(&self) -> bool {
        self.might_yield
    }

    /// Get the unique memory address for this coroutine.
    pub(crate) fn addr(&self) -> usize {
        &*self.poller as *const dyn CoroutinePoller as *const () as usize
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
    }

    /// Unwrap the original closure the coroutine was created from. Panics if
    /// the coroutine was not created from a closure.
    pub(crate) fn into_inner_closure<F, T>(self) -> F
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        (*self
            .poller
            .into_any()
            .downcast::<ClosurePoller<F, T>>()
            .unwrap())
        .closure
        .take()
        .unwrap()
    }

    /// Unwrap the original future the coroutine was created from. Panics if the
    /// coroutine was not created from a future.
    pub(crate) fn into_inner_future<F, T>(self) -> F
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        (*self
            .poller
            .into_any()
            .downcast::<FuturePoller<F, T>>()
            .unwrap())
        .future
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

/// Inner implementation of a coroutine. This trait is used to erase the return
/// value from the coroutine type as well as to abstract over futures and
/// synchronous closures. Bundling all the required operations into this trait
/// also allows us to minimize the number of heap allocations per task.
trait CoroutinePoller: Send + 'static {
    fn run(&mut self, cx: &mut Context) -> RunResult;

    fn complete(&mut self);

    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

struct ClosurePoller<F, T> {
    closure: Option<F>,
    result: Option<thread::Result<T>>,
    task: Arc<Mutex<Inner<T>>>,
}

impl<F, T> CoroutinePoller for ClosurePoller<F, T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    fn run(&mut self, _cx: &mut Context) -> RunResult {
        let closure = self
            .closure
            .take()
            .expect("closure already ran to completion");
        let result = catch_unwind(AssertUnwindSafe(closure));
        let panicked = result.is_err();

        self.result = Some(result);

        RunResult::Complete {
            panicked,
        }
    }

    fn complete(&mut self) {
        if let Some(result) = self.result.take() {
            let mut task = self.task.lock().unwrap();

            task.result = Some(result);

            if let Some(waker) = task.waker.as_ref() {
                waker.wake_by_ref();
            };
        }
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

struct FuturePoller<F, T> {
    future: F,
    result: Option<thread::Result<T>>,
    task: Arc<Mutex<Inner<T>>>,
}

impl<F, T> CoroutinePoller for FuturePoller<F, T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    fn run(&mut self, cx: &mut Context) -> RunResult {
        // Safety: This struct is only ever used inside a box, so we know that
        // neither self nor this future will move.
        let future = unsafe { Pin::new_unchecked(&mut self.future) };

        match catch_unwind(AssertUnwindSafe(|| future.poll(cx))) {
            Ok(Poll::Pending) => RunResult::Yield,
            Ok(Poll::Ready(value)) => {
                self.result = Some(Ok(value));

                RunResult::Complete {
                    panicked: false,
                }
            }
            Err(e) => {
                self.result = Some(Err(e));

                RunResult::Complete {
                    panicked: true,
                }
            }
        }
    }

    fn complete(&mut self) {
        if let Some(result) = self.result.take() {
            let mut task = self.task.lock().unwrap();

            task.result = Some(result);

            if let Some(waker) = task.waker.as_ref() {
                waker.wake_by_ref();
            };
        }
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}
