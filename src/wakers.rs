use std::{
    ptr,
    sync::Arc,
    task::{RawWaker, RawWakerVTable, Wake, Waker},
    thread::{self, Thread},
};

/// Creates a dummy waker that does nothing.
pub(crate) fn empty_waker() -> Waker {
    const RAW_WAKER: RawWaker = RawWaker::new(ptr::null(), &VTABLE);
    const VTABLE: RawWakerVTable = RawWakerVTable::new(|_| RAW_WAKER, |_| {}, |_| {}, |_| {});

    unsafe { Waker::from_raw(RAW_WAKER) }
}

/// Creates a waker that unparks the current thread.
pub(crate) fn current_thread_waker() -> Waker {
    thread_waker(thread::current())
}

/// Creates a waker that unparks a thread.
pub(crate) fn thread_waker(thread: Thread) -> Waker {
    struct ThreadWaker(Thread);

    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    Arc::new(ThreadWaker(thread)).into()
}
