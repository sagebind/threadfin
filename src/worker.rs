use std::{sync::Arc, task::Wake, time::Duration};

use crossbeam_channel::{select, unbounded, Receiver, Sender};
use slab::Slab;

use crate::task::Coroutine;

/// A type which receives notifications from a worker.
pub(crate) trait Listener {
    fn on_task_started(&mut self) {}

    fn on_task_completed(&mut self) {}
}

/// A worker thread which belongs to a thread pool and executes tasks.
pub(crate) struct Worker<L: Listener> {
    idle_timeout: Duration,

    /// An initial task this worker should be run before polling for new work.
    initial_task: Option<Coroutine>,

    /// Pending tasks being run by this worker. Any task that yields without
    /// being immediately complete is moved to this location to be polled again.
    ///
    /// TODO: Should we set a limit on concurrency per worker?
    pending_tasks: Slab<Coroutine>,

    /// Queue of new tasks to run. The worker pulls more tasks from this queue
    /// when idle.
    queue: Receiver<Coroutine>,
    immediate_queue: Receiver<Coroutine>,

    /// Channel used to receive notifications from wakers for pending tasks.
    wake_notifications: (Sender<usize>, Receiver<usize>),

    /// Receiver of various worker events.
    listener: L,
}

impl<L: Listener> Worker<L> {
    pub(crate) fn new(
        initial_task: Option<Coroutine>,
        queue: Receiver<Coroutine>,
        immediate_queue: Receiver<Coroutine>,
        idle_timeout: Duration,
        listener: L,
    ) -> Self {
        Self {
            idle_timeout,
            initial_task,
            pending_tasks: Slab::new(),
            queue,
            immediate_queue,
            wake_notifications: unbounded(),
            listener,
        }
    }

    pub(crate) fn run(&mut self) {
        if let Some(runner) = self.initial_task.take() {
            self.run_now_or_reschedule(runner);
        }

        // Main worker loop
        loop {
            select! {
                recv(self.queue) -> runner => {
                    if let Ok(runner) = runner {
                        self.run_now_or_reschedule(runner);
                    } else {
                        // todo!("pool closed, shut down worker");
                        break;
                    }
                }
                recv(self.immediate_queue) -> runner => {
                    if let Ok(runner) = runner {
                        self.run_now_or_reschedule(runner);
                    } else {
                        // todo!("pool closed, shut down worker");
                        break;
                    }
                }
                recv(self.wake_notifications.1) -> id => {
                    let id = id.expect("wake channel can't be dropped");
                    self.run_by_id(id);
                }
                default(self.idle_timeout) => {
                    // todo!("terminate worker if over min_threads");
                    return;
                }
            }
        }

        // Worker has been instructed to stop, but we want to finish running any
        // pending tasks we still may have.
        while !self.pending_tasks.is_empty() {
            let id = self
                .wake_notifications
                .1
                .recv()
                .expect("wake channel can't be dropped");
            self.run_by_id(id);
        }
    }

    fn run_now_or_reschedule(&mut self, mut runner: Coroutine) {
        let vacant_entry = self.pending_tasks.vacant_entry();

        // If it is possible for this task to yield, we need to prepare a new
        // waker to receive notifications with.
        if runner.might_yield() {
            struct IdWaker(usize, Sender<usize>);

            impl Wake for IdWaker {
                fn wake(self: Arc<Self>) {
                    self.wake_by_ref();
                }

                fn wake_by_ref(self: &Arc<Self>) {
                    let _ = self.1.send(self.0);
                }
            }

            runner.set_waker(
                Arc::new(IdWaker(
                    vacant_entry.key(),
                    self.wake_notifications.0.clone(),
                ))
                .into(),
            );
        }

        self.listener.on_task_started();

        if runner.run() {
            self.listener.on_task_completed();
            runner.notify();
        } else {
            // This should never happen if the task promised not to yield!
            debug_assert!(runner.might_yield());

            // Task yielded, so we'll need to reschedule the task to be polled
            // again when its waker is called.
            vacant_entry.insert(runner);
        }
    }

    fn run_by_id(&mut self, id: usize) {
        if let Some(runner) = self.pending_tasks.get_mut(id) {
            if runner.run() {
                self.listener.on_task_completed();
                runner.notify();

                // Task is complete, we can de-allocate it.
                self.pending_tasks.remove(id);
            }
        }
    }
}
