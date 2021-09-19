//! A thread pool designed for background value computation.

use std::{
    fmt,
    future::Future,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use core_affinity::CoreId;
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};

mod task;
mod wakers;
mod worker;

use private::ThreadPoolSize;
use task::Coroutine;
pub use task::Task;

use crate::worker::Listener;

/// A builder for constructing a customized thread pool.
#[derive(Debug)]
pub struct ThreadPoolBuilder {
    name: Option<String>,
    size: Option<ThreadPoolSize>,
    stack_size: Option<usize>,
    queue_limit: Option<usize>,
    worker_concurrency_limit: usize,
    idle_timeout: Duration,
}

impl Default for ThreadPoolBuilder {
    fn default() -> Self {
        Self {
            name: None,
            size: None,
            stack_size: None,
            queue_limit: None,
            worker_concurrency_limit: 16,
            idle_timeout: Duration::from_secs(60),
        }
    }
}

impl ThreadPoolBuilder {
    /// Set a custom thread name for threads spawned by this thread pool.
    ///
    /// # Panics
    ///
    /// Panics if the name contains null bytes (`\0`).
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
    /// If not set, a reasonable size will be selected based on the number of
    /// CPU cores on the current system.
    ///
    /// # Examples
    ///
    /// ```
    /// // Create a thread pool with exactly 2 threads.
    /// # use skipper::ThreadPool;
    /// let pool = ThreadPool::builder().size(2).build();
    /// ```
    ///
    /// ```
    /// // Create a thread pool with no idle threads, but will spawn up to 4
    /// // threads lazily when there's work to be done.
    /// # use skipper::ThreadPool;
    /// let pool = ThreadPool::builder().size(0..4).build();
    ///
    /// // Or equivalently:
    /// let pool = ThreadPool::builder().size(..4).build();
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if an invalid range is supplied with a lower bound larger than
    /// the upper bound, or if the upper bound is 0.
    pub fn size<T: Into<ThreadPoolSize>>(mut self, size: T) -> Self {
        let size = size.into();

        if size.min > size.max {
            panic!("thread pool minimum size cannot be larger than maximum size");
        }

        if size.max == 0 {
            panic!("thread pool maximum size must be non-zero");
        }

        self.size = Some(size);
        self
    }

    /// Set the size of the stack (in bytes) for threads in this thread pool.
    ///
    /// The actual stack size may be greater than this value if the platform
    /// enforces a larger minimum stack size.
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

    /// Set a timeout for idle worker threads.
    ///
    /// If the pool has more than the minimum configured number of threads and
    /// threads remain idle for more than this duration, they will be terminated
    /// until the minimum thread count is reached.
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
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
        let core_ids = core_affinity::get_core_ids();

        let size = self.size.unwrap_or_else(|| {
            let cpus = core_ids
                .as_ref()
                .filter(|v| !v.is_empty())
                .map(Vec::len)
                .unwrap_or(1);

            ThreadPoolSize {
                min: cpus,
                max: cpus * 2,
            }
        });

        let shared = Shared {
            size,
            thread_count: Default::default(),
            running_tasks_count: Default::default(),
            completed_tasks_count: Default::default(),
            panicked_tasks_count: Default::default(),
            idle_timeout: self.idle_timeout,
            shutdown_cvar: Condvar::new(),
        };

        let pool = ThreadPool {
            thread_name: self.name,
            stack_size: self.stack_size,
            concurrency_limit: self.worker_concurrency_limit,
            queue: self.queue_limit.map(bounded).unwrap_or_else(unbounded),
            immediate_queue: bounded(0),
            core_ids,
            shared: Arc::new(shared),
        };

        for _ in 0..size.min {
            let result = pool.spawn_thread(None);
            assert!(result.is_ok());
        }

        pool
    }
}

/// A thread pool.
///
/// Dropping the thread pool will prevent any further tasks from being scheduled
/// on the pool and detaches all threads in the pool. If you want to block until
/// all pending tasks have completed and the pool is entirely shut down, then
/// use [`ThreadPool::join`].
pub struct ThreadPool {
    thread_name: Option<String>,
    stack_size: Option<usize>,
    concurrency_limit: usize,
    queue: (Sender<Coroutine>, Receiver<Coroutine>),
    immediate_queue: (Sender<Coroutine>, Receiver<Coroutine>),
    core_ids: Option<Vec<CoreId>>,
    shared: Arc<Shared>,
}

impl Default for ThreadPool {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadPool {
    /// Get a shared reference to a common, shared thread pool for the current
    /// process.
    #[cfg(feature = "common")]
    pub fn common() -> &'static Self {
        use once_cell::sync::OnceCell;

        static COMMON: OnceCell<ThreadPool> = OnceCell::new();

        COMMON.get_or_init(Self::default)
    }

    /// Create a new thread pool with the default configuration.
    #[inline]
    pub fn new() -> Self {
        Self::builder().build()
    }

    /// Get a builder for creating a customized thread pool.
    #[inline]
    pub fn builder() -> ThreadPoolBuilder {
        ThreadPoolBuilder::default()
    }

    /// Get the number of threads currently running in the thread pool.
    pub fn threads(&self) -> usize {
        *self.shared.thread_count.lock().unwrap()
    }

    /// Get the number of tasks queued for execution, but not yet started.
    ///
    /// This number will always be less than or equal to the configured
    /// [`queue_limit`][ThreadPoolBuilder::queue_limit], if any.
    pub fn queued_tasks(&self) -> usize {
        self.queue.0.len()
    }

    /// Get the number of tasks currently running.
    pub fn running_tasks(&self) -> usize {
        self.shared.running_tasks_count.load(Ordering::SeqCst)
    }

    /// Get the number of tasks completed (successfully or otherwise) by this pool since it was created.
    pub fn completed_tasks(&self) -> u64 {
        self.shared.completed_tasks_count.load(Ordering::SeqCst)
    }

    /// Get the number of tasks that have panicked since the pool was created.
    pub fn panicked_tasks(&self) -> u64 {
        self.shared.panicked_tasks_count.load(Ordering::SeqCst)
    }

    /// Submit a task to be executed.
    ///
    /// If all worker threads are busy, but there are less threads than the
    /// configured maximum, an additional thread will be created and added to
    /// the pool to execute this task.
    ///
    /// If all worker threads are busy and the pool has reached the configured
    /// maximum number of threads, the task will be enqueued. If the queue is
    /// configured with a limit, this call will block until space becomes
    /// available in the queue.
    pub fn execute<T, F>(&self, closure: F) -> Task<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (task, coroutine) = Task::from_closure(closure);

        self.execute_coroutine(coroutine);

        task
    }

    /// Execute a future on the thread pool.
    pub fn execute_future<T, F>(&self, future: F) -> Task<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let (task, coroutine) = Task::from_future(future);

        self.execute_coroutine(coroutine);

        task
    }

    /// Execute a closure on the thread pool without blocking.
    ///
    /// If the task queue is full, the task is rejected and `None` is returned.
    pub fn try_execute<T, F>(&self, closure: F) -> Option<Task<T>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (task, coroutine) = Task::from_closure(closure);

        self.try_execute_coroutine(coroutine).ok().map(|_| task)
    }

    /// Execute a future on the thread pool.
    ///
    /// If the task queue is full, the task is rejected and `None` is returned.
    pub fn try_execute_future<T, F>(&self, future: F) -> Option<Task<T>>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let (task, coroutine) = Task::from_future(future);

        self.try_execute_coroutine(coroutine).ok().map(|_| task)
    }

    fn execute_coroutine(&self, coroutine: Coroutine) {
        if let Err(coroutine) = self.try_execute_coroutine(coroutine) {
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
                    .fetch_add(1, Ordering::SeqCst);
            }

            fn on_task_completed(&mut self, panicked: bool) {
                self.shared
                    .running_tasks_count
                    .fetch_sub(1, Ordering::SeqCst);
                self.shared
                    .completed_tasks_count
                    .fetch_add(1, Ordering::SeqCst);

                if panicked {
                    self.shared
                        .panicked_tasks_count
                        .fetch_add(1, Ordering::SeqCst);
                }
            }

            fn on_idle(&mut self) -> bool {
                // Check if the worker should shut down by seeing if we are over
                // the minimum worker count.
                *self.shared.thread_count.lock().unwrap() > self.shared.size.min
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
        if *thread_count >= self.shared.size.max {
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

        let mut core_id = None;

        // Select the core to pin this worker to in a deterministic round-robin
        // fashion. For example, if the system has 2 CPU cores, threads 0, 2, 4,
        // etc will be pinned to core 0, and threads 1, 3, 5, etc will be pinned
        // to core 1.
        //
        // We only do this if the min number of threads is at least as large as
        // the number of cores.
        if let Some(core_ids) = self.core_ids.as_ref() {
            if !core_ids.is_empty() && self.shared.size.min >= core_ids.len() {
                core_id = Some(core_ids[*thread_count % core_ids.len()]);
            }
        }

        *thread_count += 1;

        let worker = worker::Worker::new(
            initial_task,
            self.queue.1.clone(),
            self.immediate_queue.1.clone(),
            self.concurrency_limit,
            self.shared.idle_timeout,
            WorkerListener {
                shared: self.shared.clone(),
            },
        );

        // We can now safely unlock the thread count since the worker struct
        // will decrement the count again if it is dropped.
        drop(thread_count);

        builder
            .spawn(move || {
                if let Some(core_id) = core_id {
                    core_affinity::set_for_current(core_id);
                }

                worker.run();
            })
            .unwrap();

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
    size: ThreadPoolSize,
    thread_count: Mutex<usize>,
    running_tasks_count: AtomicUsize,
    completed_tasks_count: AtomicU64,
    panicked_tasks_count: AtomicU64,
    idle_timeout: Duration,
    shutdown_cvar: Condvar,
}

mod private {
    use std::ops::{Range, RangeTo};

    #[derive(Copy, Clone, Debug)]
    pub struct ThreadPoolSize {
        pub(crate) min: usize,
        pub(crate) max: usize,
    }

    impl From<usize> for ThreadPoolSize {
        fn from(size: usize) -> Self {
            Self {
                min: size,
                max: size,
            }
        }
    }

    impl From<Range<usize>> for ThreadPoolSize {
        fn from(range: Range<usize>) -> Self {
            Self {
                min: range.start,
                max: range.end,
            }
        }
    }

    impl From<RangeTo<usize>> for ThreadPoolSize {
        fn from(range: RangeTo<usize>) -> Self {
            Self {
                min: 0,
                max: range.end,
            }
        }
    }
}
