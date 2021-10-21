use std::{collections::HashMap, sync::Arc, task::Wake, time::Duration};

use crossbeam_channel::{unbounded, Receiver, Select, Sender};

use crate::task::{Coroutine, RunResult};

/// A type which receives notifications from a worker.
pub(crate) trait Listener {
    fn on_task_started(&mut self) {}

    fn on_task_completed(&mut self, _panicked: bool) {}

    fn on_idle(&mut self) -> bool {
        true
    }
}

/// A worker thread which belongs to a thread pool and executes tasks.
pub(crate) struct Worker<L: Listener> {
    keep_alive: Duration,

    concurrency_limit: usize,

    /// An initial task this worker should be run before polling for new work.
    initial_task: Option<Coroutine>,

    /// Pending tasks being run by this worker. Any task that yields without
    /// being immediately complete is moved to this location to be polled again.
    pending_tasks: HashMap<usize, Coroutine>,

    /// Queue of new tasks to run. The worker pulls more tasks from this queue
    /// when idle.
    queue: Receiver<Coroutine>,
    immediate_queue: Receiver<Coroutine>,

    /// Channel used to receive notifications from wakers for pending tasks.
    wake_notifications: (Sender<usize>, Receiver<usize>),

    /// Set to true when the worker is running and wants to consume more work.
    active: bool,

    /// Receiver of various worker events.
    listener: L,
}

impl<L: Listener> Worker<L> {
    /// Create a new worker.
    pub(crate) fn new(
        initial_task: Option<Coroutine>,
        queue: Receiver<Coroutine>,
        immediate_queue: Receiver<Coroutine>,
        concurrency_limit: usize,
        keep_alive: Duration,
        listener: L,
    ) -> Self {
        Self {
            keep_alive,
            concurrency_limit,
            initial_task,
            pending_tasks: HashMap::new(),
            queue,
            immediate_queue,
            wake_notifications: unbounded(),
            active: false,
            listener,
        }
    }

    /// Run the worker on the current thread until the work queue is closed.
    pub(crate) fn run(mut self) {
        self.active = true;

        if let Some(coroutine) = self.initial_task.take() {
            self.run_now_or_reschedule(coroutine);
        }

        // Main worker loop, keep running until the pool shuts down and pending
        // tasks complete.
        while self.active || !self.pending_tasks.is_empty() {
            match self.poll_work() {
                PollResult::Work(coroutine) => self.run_now_or_reschedule(coroutine),
                PollResult::Wake(id) => self.run_pending_by_id(id),
                PollResult::ShutDown => self.active = false,
                PollResult::Timeout => {
                    // If this worker doesn't have an pending tasks, then we can
                    // potentially shut down the worker due to inactivity.
                    if self.pending_tasks.is_empty() {
                        // If the listener tells us we ought to shut down, then
                        // do so.
                        if self.listener.on_idle() {
                            self.active = false;
                        }
                    }
                }
            }
        }
    }

    /// Poll for the next work item the worker should work on.
    fn poll_work(&mut self) -> PollResult {
        let mut queue_id = None;
        let mut immediate_queue_id = None;
        let mut wake_id = None;
        let mut select = Select::new();

        // As long as we haven't reached our concurrency limit, poll for
        // additional work.
        if self.active && self.pending_tasks.len() < self.concurrency_limit {
            queue_id = Some(select.recv(&self.queue));
            immediate_queue_id = Some(select.recv(&self.immediate_queue));
        }

        // If we have pending tasks, poll for waker notifications as well.
        if !self.pending_tasks.is_empty() {
            wake_id = Some(select.recv(&self.wake_notifications.1));
        }

        match select.select_timeout(self.keep_alive) {
            Ok(op) if Some(op.index()) == queue_id => {
                if let Ok(coroutine) = op.recv(&self.queue) {
                    PollResult::Work(coroutine)
                } else {
                    PollResult::ShutDown
                }
            }
            Ok(op) if Some(op.index()) == immediate_queue_id => {
                if let Ok(coroutine) = op.recv(&self.immediate_queue) {
                    PollResult::Work(coroutine)
                } else {
                    PollResult::ShutDown
                }
            }
            Ok(op) if Some(op.index()) == wake_id => {
                PollResult::Wake(op.recv(&self.wake_notifications.1).unwrap())
            }
            Ok(_) => unreachable!(),
            Err(_) => PollResult::Timeout,
        }
    }

    fn run_now_or_reschedule(&mut self, mut coroutine: Coroutine) {
        // If it is possible for this task to yield, we need to prepare a new
        // waker to receive notifications with.
        if coroutine.might_yield() {
            struct IdWaker(usize, Sender<usize>);

            impl Wake for IdWaker {
                fn wake(self: Arc<Self>) {
                    self.wake_by_ref();
                }

                fn wake_by_ref(self: &Arc<Self>) {
                    let _ = self.1.send(self.0);
                }
            }

            coroutine.set_waker(
                Arc::new(IdWaker(coroutine.addr(), self.wake_notifications.0.clone())).into(),
            );
        }

        self.listener.on_task_started();

        if let RunResult::Complete {
            panicked,
        } = coroutine.run()
        {
            self.listener.on_task_completed(panicked);
            coroutine.complete();
        } else {
            // This should never happen if the task promised not to yield!
            debug_assert!(coroutine.might_yield());

            // Task yielded, so we'll need to reschedule the task to be polled
            // again when its waker is called. We do this by storing the future
            // in a collection local to this worker where we can retrieve it
            // again.
            //
            // The benefit of doing it this way instead of sending the future
            // back through the queue is that the future gets executed (almost)
            // immediately once it wakes instead of being put behind a queue of
            // _new_ tasks.
            self.pending_tasks.insert(coroutine.addr(), coroutine);
        }
    }

    fn run_pending_by_id(&mut self, id: usize) {
        if let Some(coroutine) = self.pending_tasks.get_mut(&id) {
            if let RunResult::Complete {
                panicked,
            } = coroutine.run()
            {
                self.listener.on_task_completed(panicked);

                // Task is complete, we can de-allocate it and complete it.
                self.pending_tasks.remove(&id).unwrap().complete();
            }
        }
    }
}

enum PollResult {
    /// New work has arrived for this worker.
    Work(Coroutine),

    /// An existing pending task has woken.
    Wake(usize),

    /// No activity occurred within the time limit.
    Timeout,

    /// The thread pool has been shut down.
    ShutDown,
}
