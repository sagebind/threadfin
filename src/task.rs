use std::{future::Future, panic::{AssertUnwindSafe, catch_unwind, resume_unwind}, pin::Pin, sync::{Arc, Weak}, task::{Context, Poll}, thread, time::Duration};

use atomic_waker::AtomicWaker;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TryRecvError};

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
    pub(crate) fn new() -> (Self, TaskCompleter<T>) {
        let (tx, rx) = bounded(1);

        let task = Task {
            receiver: rx,
            waker: Arc::new(AtomicWaker::new()),
        };

        let completer = TaskCompleter {
            sender: Some(tx),
            waker: Arc::downgrade(&task.waker),
        };

        (task, completer)
    }

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
            Err(RecvTimeoutError::Disconnected) => panic!("task was dropped by thread pool without being completed"),
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
            Err(TryRecvError::Disconnected) => panic!("task was dropped by thread pool without being completed"),
        }
    }
}

pub(crate) struct TaskCompleter<T: Send> {
    sender: Option<Sender<thread::Result<T>>>,
    waker: Weak<AtomicWaker>,
}

impl<T: Send> TaskCompleter<T> {
    pub(crate) fn complete(mut self, f: impl FnOnce() -> T) {
        if let Some(waker) = self.waker.upgrade() {
            if let Some(sender) = self.sender.take() {
                let result = catch_unwind(AssertUnwindSafe(f));

                if sender.send(result).is_ok() {
                    waker.wake();
                }
            }
        } else {
            log::trace!("task canceled before it could run, ignoring");
        }
    }
}
