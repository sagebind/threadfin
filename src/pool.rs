//! Implementation of the thread pool itself.

use std::{
    fmt,
    future::Future,
    ops::{Range, RangeInclusive, RangeTo, RangeToInclusive},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
        Condvar,
        Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use once_cell::sync::Lazy;

use crate::{
    error::PoolFullError,
    task::{Coroutine, Task},
    worker::{Listener, Worker},
};

#[cfg(target_has_atomic = "64")]
type AtomicCounter = std::sync::atomic::AtomicU64;

#[cfg(not(target_has_atomic = "64"))]
type AtomicCounter = std::sync::atomic::AtomicU32;

/// A value describing a size constraint for a thread pool.
///
/// Any size constraint can be wrapped in [`PerCore`] to be made relative to the
/// number of available CPU cores on the current system.
///
/// See [`Builder::size`] for details.
pub trait SizeConstraint {
    /// Get the minimum number of threads to be in the thread pool.
    fn min(&self) -> usize;

    /// Get the maximum number of threads to be in the thread pool.
    fn max(&self) -> usize;
}

impl SizeConstraint for usize {
    fn min(&self) -> usize {
        *self
    }

    fn max(&self) -> usize {
        *self
    }
}

impl SizeConstraint for Range<usize> {
    fn min(&self) -> usize {
        self.start
    }

    fn max(&self) -> usize {
        self.end
    }
}

impl SizeConstraint for RangeInclusive<usize> {
    fn min(&self) -> usize {
        *self.start()
    }

    fn max(&self) -> usize {
        *self.end()
    }
}

impl SizeConstraint for RangeTo<usize> {
    fn min(&self) -> usize {
        0
    }

    fn max(&self) -> usize {
        self.end
    }
}

impl SizeConstraint for RangeToInclusive<usize> {
    fn min(&self) -> usize {
        0
    }

    fn max(&self) -> usize {
        self.end
    }
}

/// Modifies a size constraint to be per available CPU core.
///
/// # Examples
///
/// ```
/// # use threadfin::PerCore;
/// // one thread per core
/// let size = PerCore(1);
///
/// // four threads per core
/// let size = PerCore(4);
///
/// // at least 1 thread per core and at most 2 threads per core
/// let size = PerCore(1..2);
/// ```
pub struct PerCore<T>(pub T);

static CORE_COUNT: Lazy<usize> = Lazy::new(|| num_cpus::get().max(1));

impl<T> From<T> for PerCore<T> {
    fn from(size: T) -> Self {
        Self(size)
    }
}

impl<T: SizeConstraint> SizeConstraint for PerCore<T> {
    fn min(&self) -> usize {
        *CORE_COUNT * self.0.min()
    }

    fn max(&self) -> usize {
        *CORE_COUNT * self.0.max()
    }
}

/// A builder for constructing a customized [`ThreadPool`].
///
/// # Examples
///
/// ```
/// let custom_pool = threadfin::builder()
///     .name("my-pool")
///     .size(2)
///     .build();
/// ```
#[derive(Debug)]
pub struct Builder {
    name: Option<String>,
    size: Option<(usize, usize)>,
    stack_size: Option<usize>,
    queue_limit: Option<usize>,
    worker_concurrency_limit: usize,
    keep_alive: Duration,
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            name: None,
            size: None,
            stack_size: None,
            queue_limit: None,
            worker_concurrency_limit: 16,
            keep_alive: Duration::from_secs(60),
        }
    }
}

impl Builder {
    /// Set a custom thread name for threads spawned by this thread pool.
    ///
    /// # Panics
    ///
    /// Panics if the name contains null bytes (`\0`).
    ///
    /// # Examples
    ///
    /// ```
    /// let pool = threadfin::builder().name("my-pool").build();
    /// ```
    pub fn name<T: Into<String>>(mut self, name: T) -> Self {
        let name = name.into();

        if name.as_bytes().contains(&0) {
            panic!("thread pool name must not contain null bytes");
        }

        self.name = Some(name);
        self
    }

    /// Set the number of threads to be managed by this thread pool.
    ///
    /// If a `usize` is supplied, the pool will have a fixed number of threads.
    /// If a range is supplied, the lower bound will be the core pool size while
    /// the upper bound will be a maximum pool size the pool is allowed to burst
    /// up to when the core threads are busy.
    ///
    /// Any size constraint can be wrapped in [`PerCore`] to be made relative to
    /// the number of available CPU cores on the current system.
    ///
    /// If not set, a reasonable size will be selected based on the number of
    /// CPU cores on the current system.
    ///
    /// # Examples
    ///
    /// ```
    /// // Create a thread pool with exactly 2 threads.
    /// let pool = threadfin::builder().size(2).build();
    /// ```
    ///
    /// ```
    /// // Create a thread pool with no idle threads, but will spawn up to 4
    /// // threads lazily when there's work to be done.
    /// let pool = threadfin::builder().size(0..4).build();
    ///
    /// // Or equivalently:
    /// let pool = threadfin::builder().size(..4).build();
    /// ```
    ///
    /// ```
    /// use threadfin::PerCore;
    ///
    /// // Create a thread pool with two threads per core.
    /// let pool = threadfin::builder().size(PerCore(2)).build();
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if an invalid range is supplied with a lower bound larger than
    /// the upper bound, or if the upper bound is 0.
    pub fn size<S: SizeConstraint>(mut self, size: S) -> Self {
        let (min, max) = (size.min(), size.max());

        if min > max {
            panic!("thread pool minimum size cannot be larger than maximum size");
        }

        if max == 0 {
            panic!("thread pool maximum size must be non-zero");
        }

        self.size = Some((min, max));
        self
    }

    /// Set the size of the stack (in bytes) for threads in this thread pool.
    ///
    /// The actual stack size may be greater than this value if the platform
    /// enforces a larger minimum stack size.
    ///
    /// The stack size if not specified will be the default size for new Rust
    /// threads, currently 2 MiB. This can also be overridden by setting the
    /// `RUST_MIN_STACK` environment variable if not specified in code.
    ///
    /// # Examples
    ///
    /// ```
    /// // Worker threads will have a stack size of at least 32 KiB.
    /// let pool = threadfin::builder().stack_size(32 * 1024).build();
    /// ```
    pub fn stack_size(mut self, size: usize) -> Self {
        self.stack_size = Some(size);
        self
    }

    /// Set a maximum number of pending tasks allowed to be submitted before
    /// blocking.
    ///
    /// If set to zero, queueing will be disabled and attempting to execute a
    /// new task will block until an idle worker thread can immediately begin
    /// executing the task or a new worker thread can be created to execute the
    /// task.
    ///
    /// If not set, no limit is enforced.
    pub fn queue_limit(mut self, limit: usize) -> Self {
        self.queue_limit = Some(limit);
        self
    }

    /// Set a duration for how long to keep idle worker threads alive.
    ///
    /// If the pool has more than the minimum configured number of threads and
    /// threads remain idle for more than this duration, they will be terminated
    /// until the minimum thread count is reached.
    pub fn keep_alive(mut self, duration: Duration) -> Self {
        self.keep_alive = duration;
        self
    }

    /// Set a limit on the number of concurrent tasks that can be run by a
    /// single worker thread.
    ///
    /// When executing asynchronous tasks, if the underlying future being
    /// executed yields, that worker thread can begin working on new tasks
    /// concurrently while waiting on the prior task to resume. This allows for
    /// a primitive M:N scheduling model that supports running significantly
    /// more futures concurrently than the number of threads in the thread pool.
    ///
    /// To prevent a worker thread from over-committing to too many tasks at
    /// once (which could result in extra latency if a task wakes but its
    /// assigned worker is too busy with other tasks) worker threads limit
    /// themselves to a maximum number of concurrent tasks. This method allows
    /// you to customize that limit.
    ///
    /// The default limit if not specified is 16.
    pub fn worker_concurrency_limit(mut self, limit: usize) -> Self {
        self.worker_concurrency_limit = limit;
        self
    }

    /// Create a thread pool according to the configuration set with this
    /// builder.
    pub fn build(self) -> ThreadPool {
        let size = self.size.unwrap_or_else(|| {
            let size = PerCore(1..2);

            (size.min(), size.max())
        });

        let shared = Shared {
            min_threads: size.0,
            max_threads: size.1,
            thread_count: Default::default(),
            running_tasks_count: Default::default(),
            completed_tasks_count: Default::default(),
            panicked_tasks_count: Default::default(),
            keep_alive: self.keep_alive,
            shutdown_cvar: Condvar::new(),
        };

        let pool = ThreadPool {
            thread_name: self.name,
            stack_size: self.stack_size,
            concurrency_limit: self.worker_concurrency_limit,
            queue: self.queue_limit.map(bounded).unwrap_or_else(unbounded),
            immediate_queue: bounded(0),
            shared: Arc::new(shared),
        };

        for _ in 0..size.0 {
            let result = pool.spawn_thread(None);
            assert!(result.is_ok());
        }

        pool
    }
}

/// A thread pool for running multiple tasks on a configurable group of threads.
///
/// Thread pools can improve performance when executing a large number of
/// concurrent tasks since the expensive overhead of spawning threads is
/// minimized as threads are re-used for multiple tasks. Thread pools are also
/// useful for controlling and limiting parallelism.
///
/// Dropping the thread pool will prevent any further tasks from being scheduled
/// on the pool and detaches all threads in the pool. If you want to block until
/// all pending tasks have completed and the pool is entirely shut down, then
/// use one of the available [`join`](ThreadPool::join) methods.
///
/// # Pool size
///
/// Every thread pool has a minimum and maximum number of worker threads that it
/// will spawn for executing tasks. This range is known as the _pool size_, and
/// affects pool behavior in the following ways:
///
/// - **Minimum size**: A guaranteed number of threads that will always be
///   created and maintained by the thread pool. Threads will be eagerly created
///   to meet this minimum size when the pool is created, and at least this many
///   threads will be kept running in the pool until the pool is shut down.
/// - **Maximum size**: A limit on the number of additional threads to spawn to
///   execute more work.
///
/// # Queueing
///
/// If a new or existing worker thread is unable to immediately start processing
/// a submitted task, that task will be placed in a queue for worker threads to
/// take from when they complete their current tasks. Queueing is only used when
/// it is not possible to directly handoff a task to an existing thread and
/// spawning a new thread would exceed the pool's configured maximum size.
///
/// By default, thread pools are configured to use an _unbounded_ queue which
/// can hold an unlimited number of pending tasks. This is a sensible default,
/// but is not desirable in all use-cases and can be changed with
/// [`Builder::queue_limit`].
///
/// # Monitoring
///
/// Each pool instance provides methods for gathering various statistics on the
/// pool's usage, such as number of current number of threads, tasks completed
/// over time, and queued tasks. While these methods provide the most up-to-date
/// numbers upon invocation, they should not be used for controlling program
/// behavior since they can become immediately outdated due to the live nature
/// of the pool.
pub struct ThreadPool {
    thread_name: Option<String>,
    stack_size: Option<usize>,
    concurrency_limit: usize,
    queue: (Sender<Coroutine>, Receiver<Coroutine>),
    immediate_queue: (Sender<Coroutine>, Receiver<Coroutine>),
    shared: Arc<Shared>,
}

impl Default for ThreadPool {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadPool {
    /// Create a new thread pool with the default configuration.
    ///
    /// If you'd like to customize the thread pool's behavior then use
    /// [`ThreadPool::builder`].
    #[inline]
    pub fn new() -> Self {
        Self::builder().build()
    }

    /// Get a builder for creating a customized thread pool.
    #[inline]
    pub fn builder() -> Builder {
        Builder::default()
    }

    /// Get the number of threads currently running in the thread pool.
    pub fn threads(&self) -> usize {
        *self.shared.thread_count.lock().unwrap()
    }

    /// Get the number of tasks queued for execution, but not yet started.
    ///
    /// This number will always be less than or equal to the configured
    /// [`queue_limit`](Builder::queue_limit), if any.
    ///
    /// Note that the number returned may become immediately outdated after
    /// invocation.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::{thread::sleep, time::Duration};
    ///
    /// // Create a pool with just one thread.
    /// let pool = threadfin::builder().size(1).build();
    ///
    /// // Nothing is queued yet.
    /// assert_eq!(pool.queued_tasks(), 0);
    ///
    /// // Start a slow task.
    /// let task = pool.execute(|| {
    ///     sleep(Duration::from_millis(100));
    /// });
    ///
    /// // Wait a little for the task to start.
    /// sleep(Duration::from_millis(10));
    /// assert_eq!(pool.queued_tasks(), 0);
    ///
    /// // Enqueue some more tasks.
    /// let count = 4;
    /// for _ in 0..count {
    ///     pool.execute(|| {
    ///         // work to do
    ///     });
    /// }
    ///
    /// // The tasks should still be in the queue because the slow task is
    /// // running on the only thread.
    /// assert_eq!(pool.queued_tasks(), count);
    /// # pool.join();
    /// ```
    #[inline]
    pub fn queued_tasks(&self) -> usize {
        self.queue.0.len()
    }

    /// Get the number of tasks currently running.
    ///
    /// Note that the number returned may become immediately outdated after
    /// invocation.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::{thread::sleep, time::Duration};
    ///
    /// let pool = threadfin::ThreadPool::new();
    ///
    /// // Nothing is running yet.
    /// assert_eq!(pool.running_tasks(), 0);
    ///
    /// // Start a task.
    /// let task = pool.execute(|| {
    ///     sleep(Duration::from_millis(100));
    /// });
    ///
    /// // Wait a little for the task to start.
    /// sleep(Duration::from_millis(10));
    /// assert_eq!(pool.running_tasks(), 1);
    ///
    /// // Wait for the task to complete.
    /// task.join();
    /// assert_eq!(pool.running_tasks(), 0);
    /// ```
    #[inline]
    pub fn running_tasks(&self) -> usize {
        self.shared.running_tasks_count.load(Ordering::Relaxed)
    }

    /// Get the number of tasks completed (successfully or otherwise) by this
    /// pool since it was created.
    ///
    /// Note that the number returned may become immediately outdated after
    /// invocation.
    ///
    /// # Examples
    ///
    /// ```
    /// let pool = threadfin::ThreadPool::new();
    /// assert_eq!(pool.completed_tasks(), 0);
    ///
    /// pool.execute(|| 2 + 2).join();
    /// assert_eq!(pool.completed_tasks(), 1);
    ///
    /// pool.execute(|| 2 + 2).join();
    /// assert_eq!(pool.completed_tasks(), 2);
    /// ```
    #[inline]
    #[allow(clippy::useless_conversion)]
    pub fn completed_tasks(&self) -> u64 {
        self.shared.completed_tasks_count.load(Ordering::Relaxed).into()
    }

    /// Get the number of tasks that have panicked since the pool was created.
    ///
    /// Note that the number returned may become immediately outdated after
    /// invocation.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::{thread::sleep, time::Duration};
    ///
    /// let pool = threadfin::ThreadPool::new();
    /// assert_eq!(pool.panicked_tasks(), 0);
    ///
    /// pool.execute(|| {
    ///     panic!("this task panics");
    /// });
    ///
    /// sleep(Duration::from_millis(100));
    ///
    /// assert_eq!(pool.panicked_tasks(), 1);
    /// ```
    #[inline]
    #[allow(clippy::useless_conversion)]
    pub fn panicked_tasks(&self) -> u64 {
        self.shared.panicked_tasks_count.load(Ordering::Relaxed).into()
    }

    /// Submit a closure to be executed by the thread pool.
    ///
    /// If all worker threads are busy, but there are less threads than the
    /// configured maximum, an additional thread will be created and added to
    /// the pool to execute this task.
    ///
    /// If all worker threads are busy and the pool has reached the configured
    /// maximum number of threads, the task will be enqueued. If the queue is
    /// configured with a limit, this call will block until space becomes
    /// available in the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// let pool = threadfin::ThreadPool::new();
    /// let task = pool.execute(|| {
    ///     2 + 2 // some expensive computation
    /// });
    ///
    /// // do something in the meantime
    ///
    /// // now wait for the result
    /// let sum = task.join();
    /// assert_eq!(sum, 4);
    /// ```
    pub fn execute<T, F>(&self, closure: F) -> Task<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (task, coroutine) = Task::from_closure(closure);

        self.execute_coroutine(coroutine);

        task
    }

    /// Submit a future to be executed by the thread pool.
    ///
    /// If all worker threads are busy, but there are less threads than the
    /// configured maximum, an additional thread will be created and added to
    /// the pool to execute this task.
    ///
    /// If all worker threads are busy and the pool has reached the configured
    /// maximum number of threads, the task will be enqueued. If the queue is
    /// configured with a limit, this call will block until space becomes
    /// available in the queue.
    ///
    /// # Thread locality
    ///
    /// While the given future must implement [`Send`] to be moved into a thread
    /// in the pool to be processed, once the future is assigned a thread it
    /// will stay assigned to that single thread until completion. This improves
    /// cache locality even across `.await` points in the future.
    ///
    /// ```
    /// let pool = threadfin::ThreadPool::new();
    /// let task = pool.execute_future(async {
    ///     2 + 2 // some asynchronous code
    /// });
    ///
    /// // do something in the meantime
    ///
    /// // now wait for the result
    /// let sum = task.join();
    /// assert_eq!(sum, 4);
    /// ```
    pub fn execute_future<T, F>(&self, future: F) -> Task<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let (task, coroutine) = Task::from_future(future);

        self.execute_coroutine(coroutine);

        task
    }

    /// Attempts to execute a closure on the thread pool without blocking.
    ///
    /// If the pool is at its max thread count and the task queue is full, the
    /// task is rejected and an error is returned. The original closure can be
    /// extracted from the error.
    ///
    /// # Examples
    ///
    /// One use for this method is implementing backpressure by executing a
    /// closure on the current thread if a pool is currently full.
    ///
    /// ```
    /// let pool = threadfin::ThreadPool::new();
    ///
    /// // Try to run a closure in the thread pool.
    /// let result = pool.try_execute(|| 2 + 2)
    ///     // If successfully submitted, block until the task completes.
    ///     .map(|task| task.join())
    ///     // If the pool was full, invoke the closure here and now.
    ///     .unwrap_or_else(|error| error.into_inner()());
    ///
    /// assert_eq!(result, 4);
    /// ```
    pub fn try_execute<T, F>(&self, closure: F) -> Result<Task<T>, PoolFullError<F>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (task, coroutine) = Task::from_closure(closure);

        self.try_execute_coroutine(coroutine)
            .map(|_| task)
            .map_err(|coroutine| PoolFullError(coroutine.into_inner_closure()))
    }

    /// Attempts to execute a future on the thread pool without blocking.
    ///
    /// If the pool is at its max thread count and the task queue is full, the
    /// task is rejected and an error is returned. The original future can be
    /// extracted from the error.
    pub fn try_execute_future<T, F>(&self, future: F) -> Result<Task<T>, PoolFullError<F>>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let (task, coroutine) = Task::from_future(future);

        self.try_execute_coroutine(coroutine)
            .map(|_| task)
            .map_err(|coroutine| PoolFullError(coroutine.into_inner_future()))
    }

    fn execute_coroutine(&self, coroutine: Coroutine) {
        if let Err(coroutine) = self.try_execute_coroutine(coroutine) {
            // Cannot fail because we hold a reference to both the channel
            // sender and receiver and it cannot be closed here.
            self.queue.0.send(coroutine).unwrap();
        }
    }

    fn try_execute_coroutine(&self, coroutine: Coroutine) -> Result<(), Coroutine> {
        // First, try to pass the coroutine to an idle worker currently polling
        // for work. This is the most favorable scenario for a task to begin
        // processing.
        if let Err(e) = self.immediate_queue.0.try_send(coroutine) {
            // Error means no workers are currently polling the queue.
            debug_assert!(!e.is_disconnected());

            // If possible, spawn an additional thread to handle the task.
            if let Err(e) = self.spawn_thread(Some(e.into_inner())) {
                // Finally as a last resort, enqueue the task into the queue,
                // but only if it isn't full.
                if let Err(e) = self.queue.0.try_send(e.unwrap()) {
                    return Err(e.into_inner());
                }
            }
        }

        Ok(())
    }

    /// Shut down this thread pool and block until all existing tasks have
    /// completed and threads have stopped.
    pub fn join(self) {
        self.join_internal(None);
    }

    /// Shut down this thread pool and block until all existing tasks have
    /// completed and threads have stopped, or until the given timeout passes.
    ///
    /// Returns `true` if the thread pool shut down fully before the timeout.
    pub fn join_timeout(self, timeout: Duration) -> bool {
        self.join_deadline(Instant::now() + timeout)
    }

    /// Shut down this thread pool and block until all existing tasks have
    /// completed and threads have stopped, or the given deadline passes.
    ///
    /// Returns `true` if the thread pool shut down fully before the deadline.
    pub fn join_deadline(self, deadline: Instant) -> bool {
        self.join_internal(Some(deadline))
    }

    fn join_internal(self, deadline: Option<Instant>) -> bool {
        // Closing this channel will interrupt any idle workers and signal to
        // all workers that the pool is shutting down.
        drop(self.queue.0);

        let mut thread_count = self.shared.thread_count.lock().unwrap();

        while *thread_count > 0 {
            // If a deadline is set, figure out how much time is remaining and
            // wait for that amount.
            if let Some(deadline) = deadline {
                if let Some(timeout) = deadline.checked_duration_since(Instant::now()) {
                    thread_count = self
                        .shared
                        .shutdown_cvar
                        .wait_timeout(thread_count, timeout)
                        .unwrap()
                        .0;
                } else {
                    return false;
                }
            }
            // If a deadline is not set, wait forever.
            else {
                thread_count = self.shared.shutdown_cvar.wait(thread_count).unwrap();
            }
        }

        true
    }

    /// Spawn an additional thread into the thread pool, if possible.
    ///
    /// If an initial thunk is given, it will be the first thunk the thread
    /// executes once ready for work.
    fn spawn_thread(&self, initial_task: Option<Coroutine>) -> Result<(), Option<Coroutine>> {
        struct WorkerListener {
            shared: Arc<Shared>,
        }

        impl Listener for WorkerListener {
            fn on_task_started(&mut self) {
                self.shared
                    .running_tasks_count
                    .fetch_add(1, Ordering::Relaxed);
            }

            fn on_task_completed(&mut self, panicked: bool) {
                self.shared
                    .running_tasks_count
                    .fetch_sub(1, Ordering::Relaxed);
                self.shared
                    .completed_tasks_count
                    .fetch_add(1, Ordering::Relaxed);

                if panicked {
                    self.shared
                        .panicked_tasks_count
                        .fetch_add(1, Ordering::Relaxed);
                }
            }

            fn on_idle(&mut self) -> bool {
                // Check if the worker should shut down by seeing if we are over
                // the minimum worker count.
                *self.shared.thread_count.lock().unwrap() > self.shared.min_threads
            }
        }

        impl Drop for WorkerListener {
            fn drop(&mut self) {
                if let Ok(mut count) = self.shared.thread_count.lock() {
                    *count = count.saturating_sub(1);
                    self.shared.shutdown_cvar.notify_all();
                }
            }
        }

        // Lock the thread count to prevent race conditions when determining
        // whether new threads can be created.
        let mut thread_count = self.shared.thread_count.lock().unwrap();

        // We've reached the configured limit for threads, do nothing.
        if *thread_count >= self.shared.max_threads {
            return Err(initial_task);
        }

        // Configure the thread based on the thread pool configuration.
        let mut builder = thread::Builder::new();

        if let Some(name) = self.thread_name.as_ref() {
            builder = builder.name(name.clone());
        }

        if let Some(size) = self.stack_size {
            builder = builder.stack_size(size);
        }

        *thread_count += 1;

        let worker = Worker::new(
            initial_task,
            self.queue.1.clone(),
            self.immediate_queue.1.clone(),
            self.concurrency_limit,
            self.shared.keep_alive,
            WorkerListener {
                shared: self.shared.clone(),
            },
        );

        // We can now safely unlock the thread count since the worker struct
        // will decrement the count again if it is dropped.
        drop(thread_count);

        builder.spawn(move || worker.run()).unwrap();

        Ok(())
    }
}

impl fmt::Debug for ThreadPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadPool")
            .field("queued_tasks", &self.queued_tasks())
            .field("running_tasks", &self.running_tasks())
            .field("completed_tasks", &self.completed_tasks())
            .finish()
    }
}

/// Thread pool state shared by the owner and the worker threads.
struct Shared {
    min_threads: usize,
    max_threads: usize,
    thread_count: Mutex<usize>,
    running_tasks_count: AtomicUsize,
    completed_tasks_count: AtomicCounter,
    panicked_tasks_count: AtomicCounter,
    keep_alive: Duration,
    shutdown_cvar: Condvar,
}
