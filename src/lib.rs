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

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use once_cell::sync::OnceCell;

mod task;
mod worker;

use private::ThreadPoolSize;
use task::Coroutine;
pub use task::Task;

use crate::worker::Listener;

const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(6);

/// A builder for constructing a customized thread pool.
#[derive(Debug, Default)]
pub struct ThreadPoolBuilder {
    size: Option<ThreadPoolSize>,
    idle_timeout: Option<Duration>,
    name: Option<String>,
    stack_size: Option<usize>,
    queue_limit: Option<usize>,
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
    /// # Examples
    ///
    /// ```
    /// // Create a thread pool with exactly 2 threads.
    /// # use squad::ThreadPool;
    /// let pool = ThreadPool::builder().size(2).build();
    /// ```
    ///
    /// ```
    /// // Create a thread pool with no idle threads, but will spawn up to 4
    /// // threads when there's work to be done.
    /// # use squad::ThreadPool;
    /// let pool = ThreadPool::builder().size(0..4).build();
    /// ```
    ///
    /// # Panics
    ///
    /// Panics if an invalid range is supplied with a lower bound larger than
    /// the upper bound.
    pub fn size<T: Into<ThreadPoolSize>>(mut self, size: T) -> Self {
        self.size = Some(size.into());
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

    /// Create a thread pool according to the configuration set with this
    /// builder.
    pub fn build(self) -> ThreadPool {
        let size = self.size.unwrap_or_else(|| {
            let cpus = num_cpus::get().max(1);

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
            idle_timeout: self.idle_timeout.unwrap_or(DEFAULT_IDLE_TIMEOUT),
            shutdown_cvar: Condvar::new(),
        };

        let pool = ThreadPool {
            thread_name: self.name,
            stack_size: self.stack_size,
            queue: self.queue_limit.map(bounded).unwrap_or_else(unbounded),
            immediate_queue: bounded(0),
            shared: Arc::new(shared),
        };

        for _ in 0..size.min {
            pool.spawn_thread(None);
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
    pub fn common() -> &'static Self {
        static COMMON: OnceCell<ThreadPool> = OnceCell::new();

        COMMON.get_or_init(|| Self::default())
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

    /// Get the number of tasks completed by this pool since it was created.
    pub fn completed_tasks(&self) -> u64 {
        self.shared.completed_tasks_count.load(Ordering::SeqCst)
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
        let (task, runner) = Task::from_closure(closure);

        self.execute_runner(runner);

        task
    }

    /// Execute a future on the thread pool.
    pub fn execute_future<T, F>(&self, future: F) -> Task<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let (task, runner) = Task::from_future(future);

        self.execute_runner(runner);

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
        let (task, runner) = Task::from_closure(closure);

        if self.try_execute_runner(runner).is_ok() {
            Some(task)
        } else {
            None
        }
    }

    fn execute_runner(&self, runner: Coroutine) {
        if let Err(runner) = self.try_execute_runner(runner) {
            self.queue.0.send(runner).unwrap();
        }
    }

    fn try_execute_runner(&self, runner: Coroutine) -> Result<(), Coroutine> {
        match self.immediate_queue.0.try_send(runner) {
            Ok(()) => Ok(()),

            // Error means queue is full.
            Err(e) => {
                debug_assert!(!e.is_disconnected());

                // If possible, spawn an additional thread to handle the task.
                // TODO: Lock thread count once
                if self.threads() < self.shared.size.max {
                    self.spawn_thread(Some(e.into_inner()));

                    Ok(())
                } else {
                    Err(e.into_inner())
                }
            }
        }
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
        // Dropping these channels will interrupt any idle workers and prevent
        // new tasks from being scheduled.
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

    /// Spawn an additional thread into the thread pool.
    ///
    /// If an initial thunk is given, it will be the first thunk the thread
    /// executes once ready for work.
    fn spawn_thread(&self, initial_task: Option<Coroutine>) {
        struct WorkerListener {
            shared: Arc<Shared>,
        }

        impl Listener for WorkerListener {
            fn on_task_started(&mut self) {
                self.shared
                    .running_tasks_count
                    .fetch_add(1, Ordering::SeqCst);
            }

            fn on_task_completed(&mut self) {
                self.shared
                    .running_tasks_count
                    .fetch_sub(1, Ordering::SeqCst);
                self.shared
                    .completed_tasks_count
                    .fetch_add(1, Ordering::SeqCst);
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

        // Configure the thread based on the thread pool configuration.
        let mut builder = thread::Builder::new();

        if let Some(name) = self.thread_name.as_ref() {
            builder = builder.name(name.clone());
        }

        if let Some(size) = self.stack_size {
            builder = builder.stack_size(size);
        }

        let mut thread_count = self.shared.thread_count.lock().unwrap();
        *thread_count += 1;

        let mut worker = worker::Worker::new(
            initial_task,
            self.queue.1.clone(),
            self.immediate_queue.1.clone(),
            self.shared.idle_timeout,
            WorkerListener {
                shared: self.shared.clone(),
            },
        );

        drop(thread_count);

        builder.spawn(move || worker.run()).unwrap();
    }
}

impl fmt::Debug for ThreadPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadPool")
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
    idle_timeout: Duration,
    shutdown_cvar: Condvar,
}

mod private {
    use std::ops::Range;

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
            if range.start > range.end {
                panic!("thread pool minimum size cannot be larger than maximum size");
            }

            Self {
                min: range.start,
                max: range.end,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "thread pool name must not contain null bytes")]
    fn test_name_with_null_bytes_panics() {
        ThreadPool::builder().name("uh\0oh").build();
    }

    #[test]
    #[should_panic(expected = "thread pool minimum size cannot be larger than maximum size")]
    fn test_invalid_size_panics() {
        ThreadPool::builder().size(2..1);
    }

    #[test]
    fn test_execute() {
        let pool = ThreadPool::default();

        let result = pool.execute(|| 2 + 2).get();

        assert_eq!(result, 4);
    }

    #[test]
    fn test_try_execute_under_core_count() {
        let pool = ThreadPool::builder().size(1).build();

        // Give some time for thread to start...
        thread::sleep(Duration::from_millis(50));
        assert_eq!(pool.threads(), 1);

        assert!(pool.try_execute(|| 2 + 2).is_some());
    }

    #[test]
    fn test_try_execute_over_core_count() {
        let pool = ThreadPool::builder().size(0..1).build();

        assert!(pool.try_execute(|| 2 + 2).is_some());
    }

    #[test]
    fn test_try_execute_over_limit() {
        let pool = ThreadPool::builder().size(0..1).queue_limit(0).build();

        assert!(pool.try_execute(|| 2 + 2).is_some());
        assert!(pool.try_execute(|| 2 + 2).is_none());
    }

    #[test]
    fn test_name() {
        let pool = ThreadPool::builder().name("foo").build();

        let name = pool
            .execute(|| thread::current().name().unwrap().to_owned())
            .get();

        assert_eq!(name, "foo");
    }

    #[test]
    #[should_panic(expected = "oh no!")]
    fn test_panic_propagates_to_task() {
        let pool = ThreadPool::default();

        pool.execute(|| panic!("oh no!")).get();
    }

    #[test]
    fn test_thread_count() {
        let pool = ThreadPool::builder().size(0..1).build();

        assert_eq!(pool.threads(), 0);

        pool.execute(|| 2 + 2).get();
        assert_eq!(pool.threads(), 1);

        let pool_with_starting_threads = ThreadPool::builder().size(1).build();

        // Give some time for thread to start...
        thread::sleep(Duration::from_millis(50));
        assert_eq!(pool_with_starting_threads.threads(), 1);
    }

    #[test]
    fn test_tasks_completed() {
        let pool = ThreadPool::default();
        assert_eq!(pool.completed_tasks(), 0);

        pool.execute(|| 2 + 2).get();
        assert_eq!(pool.completed_tasks(), 1);

        pool.execute(|| 2 + 2).get();
        assert_eq!(pool.completed_tasks(), 2);
    }

    #[test]
    fn test_join() {
        // Just a dumb test to make sure join doesn't do anything strange.
        ThreadPool::default().join();
    }

    #[test]
    fn test_join_timeout_expiring() {
        let pool = ThreadPool::builder().size(1).build();
        assert_eq!(pool.threads(), 1);

        // Schedule a slow task on the only thread. We have to keep the task
        // around, because dropping it could cancel the task.
        let _task = pool.execute(|| thread::sleep(Duration::from_millis(50)));

        // Joining should time out since there's one task still running longer
        // than our join timeout.
        assert!(!pool.join_timeout(Duration::from_millis(10)));
    }
}
