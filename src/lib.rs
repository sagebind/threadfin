use std::{
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{atomic::AtomicUsize, Arc},
    task::{Context, Poll},
    thread,
    time::Duration,
};

use atomic_waker::AtomicWaker;
use crossbeam_channel::{
    bounded, select, unbounded, Receiver, RecvTimeoutError, Sender, TryRecvError,
};
use crossbeam_utils::atomic::AtomicCell;
use once_cell::sync::OnceCell;

#[derive(Debug, Default)]
pub struct ThreadPoolBuilder {
    min_threads: Option<usize>,
    max_threads: Option<usize>,
    name: Option<String>,
    stack_size: Option<usize>,
}

impl ThreadPoolBuilder {
    pub fn name<T: Into<String>>(mut self, name: T) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn build(self) -> ThreadPool {
        let min_threads = self.min_threads.unwrap_or_else(num_cpus::get);
        let max_threads = self.max_threads.unwrap_or(min_threads * 2);

        let inner = Inner {
            min_threads,
            max_threads,
            thread_count: Default::default(),
            completed_tasks_count: Default::default(),
        };

        let pool = ThreadPool {
            thread_name: self.name,
            stack_size: self.stack_size,
            immediate_channel: bounded(0),
            overflow_channel: unbounded(),
            inner: Arc::new(inner),
        };

        for _ in 0..min_threads {
            pool.spawn_thread(None);
        }

        pool
    }
}

#[derive(Debug)]
pub struct ThreadPool {
    thread_name: Option<String>,
    stack_size: Option<usize>,
    immediate_channel: (Sender<Thunk>, Receiver<Thunk>),
    overflow_channel: (Sender<Thunk>, Receiver<Thunk>),
    inner: Arc<Inner>,
}

impl Default for ThreadPool {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl ThreadPool {
    pub fn common() -> &'static Self {
        static COMMON: OnceCell<ThreadPool> = OnceCell::new();

        COMMON.get_or_init(|| Self::default())
    }

    pub fn builder() -> ThreadPoolBuilder {
        ThreadPoolBuilder::default()
    }

    /// Get the number of tasks completed since the pool was created.
    pub fn completed_tasks(&self) -> u64 {
        self.inner.completed_tasks_count.load()
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
                if self.inner.thread_count.load() < self.inner.max_threads {
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
        let immediate_receiver = self.immediate_channel.1.clone();
        let overflow_receiver = self.overflow_channel.1.clone();
        let inner = self.inner.clone();

        let mut builder = thread::Builder::new();

        if let Some(name) = self.thread_name.as_ref() {
            builder = builder.name(name.clone());
        }

        if let Some(size) = self.stack_size {
            builder = builder.stack_size(size);
        }

        builder
            .spawn(move || {
                inner.thread_count.fetch_add(1);

                if let Some(thunk) = initial_thunk {
                    thunk.execute();
                    inner.completed_tasks_count.fetch_add(1);
                }

                // Select will return an error if the sender is disconnected. We
                // want to stop naturally since this means the pool has been shut
                // down.
                while let Ok(thunk) = select! {
                    recv(immediate_receiver) -> thunk => thunk,
                    recv(overflow_receiver) -> thunk => thunk,
                } {
                    thunk.execute();
                    inner.completed_tasks_count.fetch_add(1);
                }

                inner.thread_count.fetch_sub(1);
            })
            .unwrap();
    }
}

fn create_task<T, F>(f: F) -> (Thunk, Task<T>)
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (value_tx, value_rx) = bounded(1);
    let waker = Arc::new(AtomicWaker::new());
    let waker_weak = Arc::downgrade(&waker);
    let task = Task {
        receiver: value_rx,
        waker: waker,
    };

    let thunk = Thunk::new(move || {
        if let Some(waker) = waker_weak.upgrade() {
            let result = catch_unwind(AssertUnwindSafe(f));

            if value_tx.send(result).is_ok() {
                waker.wake();
            }
        } else {
            log::trace!("task canceled before it could run, ignoring");
        }
    });

    (thunk, task)
}

#[derive(Debug)]
struct Inner {
    min_threads: usize,
    max_threads: usize,
    thread_count: AtomicCell<usize>,
    completed_tasks_count: AtomicCell<u64>,
}

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

    pub fn get(self) -> thread::Result<T> {
        self.receiver.recv().unwrap()
    }

    pub fn get_timeout(self, timeout: Duration) -> Result<thread::Result<T>, Self> {
        match self.receiver.recv_timeout(timeout) {
            Ok(result) => Ok(result),
            Err(RecvTimeoutError::Timeout) => Err(self),
            Err(e) => Ok(Err(Box::new(e))),
        }
    }
}

impl<T: Send> Future for Task<T> {
    type Output = thread::Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.waker.register(cx.waker());

        match self.receiver.try_recv() {
            Ok(result) => Poll::Ready(result),
            Err(TryRecvError::Empty) => Poll::Pending,
            Err(e) => Poll::Ready(Err(Box::new(e))),
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execute() {
        let pool = ThreadPool::default();

        let result = pool.execute(|| 2 + 2).get().unwrap();

        assert_eq!(result, 4);
    }

    #[test]
    fn test_name() {
        let pool = ThreadPool::builder().name("foo").build();

        let name = pool
            .execute(|| thread::current().name().unwrap().to_owned())
            .get()
            .unwrap();

        assert_eq!(name, "foo");
    }

    #[test]
    fn test_panic() {
        let pool = ThreadPool::default();

        let result = pool.execute(|| panic!("oh no!")).get();

        assert!(result.is_err());
    }
}
