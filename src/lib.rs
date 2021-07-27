use std::{future::Future, sync::Arc, thread, time::Duration};

use crossbeam_channel::{bounded, select, unbounded, Receiver, Sender};
use crossbeam_utils::atomic::AtomicCell;
use once_cell::sync::OnceCell;

mod task;

pub use task::Task;

const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub struct ThreadPoolBuilder {
    min_threads: Option<usize>,
    max_threads: Option<usize>,
    idle_timeout: Option<Duration>,
    name: Option<String>,
    stack_size: Option<usize>,
}

impl ThreadPoolBuilder {
    /// Set a custom thread name for threads spawned by this thread pool.
    ///
    /// The name must not contain null bytes (`\0`).
    pub fn name<T: Into<String>>(mut self, name: T) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set the minimum number of threads.
    ///
    /// Panics if larger than `max_threads`.
    pub fn min_threads(mut self, size: usize) -> Self {
        if let Some(max) = self.max_threads {
            if size > max {
                panic!("thread pool min_threads cannot be larger than max_threads");
            }
        }

        self.min_threads = Some(size);
        self
    }

    /// Set the maximum number of threads.
    ///
    /// Panics if smaller than `min_threads`.
    pub fn max_threads(mut self, size: usize) -> Self {
        if let Some(min) = self.min_threads {
            if size < min {
                panic!("thread pool max_threads cannot be smaller than min_threads");
            }
        }

        self.max_threads = Some(size);
        self
    }

    /// Set the size of the stack (in bytes) for threads in this thread pool.
    ///
    /// The actual stack size may be greater than this value if the platform
    /// specifies a minimal stack size.
    pub fn stack_size(mut self, size: usize) -> Self {
        self.stack_size = Some(size);
        self
    }

    /// Create a thread pool according to the configuration set with this builder.
    pub fn build(self) -> ThreadPool {
        let min_threads = self.min_threads.unwrap_or_else(num_cpus::get);
        let max_threads = self.max_threads.unwrap_or(min_threads * 2);

        let shared = Shared {
            min_threads,
            max_threads,
            thread_count: Default::default(),
            completed_tasks_count: Default::default(),
            idle_timeout: self.idle_timeout.unwrap_or(DEFAULT_IDLE_TIMEOUT),
        };

        let pool = ThreadPool {
            thread_name: self.name,
            stack_size: self.stack_size,
            immediate_channel: bounded(0),
            overflow_channel: unbounded(),
            shared: Arc::new(shared),
        };

        for _ in 0..min_threads {
            pool.spawn_thread(None);
        }

        pool
    }
}

/// A thread pool.
#[derive(Debug)]
pub struct ThreadPool {
    thread_name: Option<String>,
    stack_size: Option<usize>,
    immediate_channel: (Sender<Thunk>, Receiver<Thunk>),
    overflow_channel: (Sender<Thunk>, Receiver<Thunk>),
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

    /// Create a new thread pool with a custom name for its threads.
    #[inline]
    pub fn with_name<T: Into<String>>(name: T) -> Self {
        Self::builder().name(name).build()
    }

    /// Get a builder for creating a customized thread pool.
    #[inline]
    pub fn builder() -> ThreadPoolBuilder {
        ThreadPoolBuilder::default()
    }

    /// Get the number of threads currently running in the thread pool.
    pub fn threads(&self) -> usize {
        self.shared.thread_count.load()
    }

    /// Get the number of tasks completed by this pool since it was created.
    pub fn completed_tasks(&self) -> u64 {
        self.shared.completed_tasks_count.load()
    }

    pub fn execute<T, F>(&self, f: F) -> Task<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (thunk, task) = create_task(f);

        if let Err(thunk) = self.try_execute_thunk(thunk) {
            // Send the task to the overflow queue.
            self.overflow_channel.0.send(thunk).unwrap();
        }

        task
    }

    /// Execute a future on the thread pool.
    pub fn execute_async<T, F>(&self, _future: F) -> Task<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        todo!()
    }

    /// Execute a closure on the thread pool, but only if a worker thread can
    /// immediately begin executing it. If no idle workers are available, `None`
    /// is returned.
    pub fn try_execute<T, F>(&self, f: F) -> Option<Task<T>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (thunk, task) = create_task(f);

        if self.try_execute_thunk(thunk).is_ok() {
            Some(task)
        } else {
            None
        }
    }

    fn try_execute_thunk(&self, thunk: Thunk) -> Result<(), Thunk> {
        // First attempt: If a worker thread is currently blocked on a recv call
        // for a thunk, then send one via the immediate channel.
        match self.immediate_channel.0.try_send(thunk) {
            Ok(()) => Ok(()),

            // Error means that no workers are currently idle.
            Err(e) => {
                debug_assert!(!e.is_disconnected());

                // If possible, spawn an additional thread to handle the task.
                if self.shared.thread_count.load() < self.shared.max_threads {
                    self.spawn_thread(Some(e.into_inner()));

                    Ok(())
                } else {
                    Err(e.into_inner())
                }
            }
        }
    }

    /// Spawn an additional thread into the thread pool.
    ///
    /// If an initial thunk is given, it will be the first thunk the thread
    /// executes once ready for work.
    fn spawn_thread(&self, initial_thunk: Option<Thunk>) {
        // Configure the thread based on the thread pool configuration.
        let mut builder = thread::Builder::new();

        if let Some(name) = self.thread_name.as_ref() {
            builder = builder.name(name.clone());
        }

        if let Some(size) = self.stack_size {
            builder = builder.stack_size(size);
        }

        self.shared.thread_count.fetch_add(1);

        let worker = Worker {
            initial_thunk,
            immediate_receiver: self.immediate_channel.1.clone(),
            overflow_receiver: self.overflow_channel.1.clone(),
            shared: self.shared.clone(),
        };

        builder.spawn(move || worker.run()).unwrap();
    }
}

fn create_task<T, F>(f: F) -> (Thunk, Task<T>)
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (task, completer) = Task::new();

    let thunk = Thunk::new(move || completer.complete(f));

    (thunk, task)
}

/// Thread pool state shared by the owner and the worker threads.
#[derive(Debug)]
struct Shared {
    min_threads: usize,
    max_threads: usize,
    thread_count: AtomicCell<usize>,
    completed_tasks_count: AtomicCell<u64>,
    idle_timeout: Duration,
}

/// Wrapper around closures that can be run by worker threads.
struct Thunk(Box<dyn FnOnce() + Send>);

impl Thunk {
    fn new(function: impl FnOnce() + Send + 'static) -> Self {
        Self(Box::new(function))
    }

    fn execute(self) {
        (self.0)()
    }
}

struct Worker {
    initial_thunk: Option<Thunk>,
    immediate_receiver: Receiver<Thunk>,
    overflow_receiver: Receiver<Thunk>,
    shared: Arc<Shared>,
}

impl Worker {
    fn run(mut self) {
        if let Some(thunk) = self.initial_thunk.take() {
            self.execute_thunk(thunk);
        }

        // Select will return an error if the sender is disconnected. We
        // want to stop naturally since this means the pool has been shut
        // down.
        while let Ok(thunk) = select! {
            recv(self.immediate_receiver) -> thunk => thunk,
            recv(self.overflow_receiver) -> thunk => thunk,
            default(self.shared.idle_timeout) => {
                todo!("terminate worker if over min_threads");
            }
        } {
            self.execute_thunk(thunk);
        }
    }

    fn execute_thunk(&self, thunk: Thunk) {
        thunk.execute();
        self.shared.completed_tasks_count.fetch_add(1);
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shared.thread_count.fetch_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "thread pool min_threads cannot be larger than max_threads")]
    fn test_setting_min_threads_larger_than_max_threads_panics() {
        ThreadPool::builder().max_threads(1).min_threads(2);
    }

    #[test]
    #[should_panic(expected = "thread pool max_threads cannot be smaller than min_threads")]
    fn test_setting_max_threads_smaller_than_min_threads_panics() {
        ThreadPool::builder().min_threads(2).max_threads(1);
    }

    #[test]
    fn test_execute() {
        let pool = ThreadPool::default();

        let result = pool.execute(|| 2 + 2).get().unwrap();

        assert_eq!(result, 4);
    }

    #[test]
    fn test_try_execute_under_core_count() {
        let pool = ThreadPool::builder().min_threads(1).max_threads(1).build();

        // Give some time for thread to start...
        thread::sleep(Duration::from_millis(50));
        assert_eq!(pool.threads(), 1);

        assert!(pool.try_execute(|| 2 + 2).is_some());
    }

    #[test]
    fn test_try_execute_over_core_count() {
        let pool = ThreadPool::builder().min_threads(0).max_threads(1).build();

        assert!(pool.try_execute(|| 2 + 2).is_some());
    }

    #[test]
    fn test_try_execute_over_max_count() {
        let pool = ThreadPool::builder().min_threads(0).max_threads(1).build();

        assert!(pool.try_execute(|| 2 + 2).is_some());
        assert!(pool.try_execute(|| 2 + 2).is_none());
    }

    #[test]
    fn test_name() {
        let pool = ThreadPool::with_name("foo");

        let name = pool
            .execute(|| thread::current().name().unwrap().to_owned())
            .get()
            .unwrap();

        assert_eq!(name, "foo");
    }

    #[test]
    fn test_panic_fails_task() {
        let pool = ThreadPool::default();

        let result = pool.execute(|| panic!("oh no!")).get();

        assert!(result.is_err());
    }

    #[test]
    fn test_thread_count() {
        let pool = ThreadPool::builder().min_threads(0).max_threads(1).build();

        assert_eq!(pool.threads(), 0);

        pool.execute(|| 2 + 2).get().unwrap();
        assert_eq!(pool.threads(), 1);

        let pool_with_starting_threads = ThreadPool::builder().min_threads(1).build();

        // Give some time for thread to start...
        thread::sleep(Duration::from_millis(50));
        assert_eq!(pool_with_starting_threads.threads(), 1);
    }

    #[test]
    fn test_tasks_completed() {
        let pool = ThreadPool::default();
        assert_eq!(pool.completed_tasks(), 0);

        pool.execute(|| 2 + 2).get().unwrap();
        assert_eq!(pool.completed_tasks(), 1);

        pool.execute(|| 2 + 2).get().unwrap();
        assert_eq!(pool.completed_tasks(), 2);
    }
}
