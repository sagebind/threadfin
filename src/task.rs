use std::{
    future::Future,
    panic::{catch_unwind, AssertUnwindSafe},
    pin::Pin,
    sync::{Arc, Weak},
    task::{Context, Poll},
    thread,
    time::Duration,
};

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
