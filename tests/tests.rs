use std::{panic::catch_unwind, thread, time::Duration};

use futures_timer::Delay;
use threadfin::ThreadPool;

fn single_thread() -> ThreadPool {
    ThreadPool::builder().size(0..1).build()
}

#[test]
#[should_panic(expected = "thread pool name must not contain null bytes")]
fn name_with_null_bytes_panics() {
    ThreadPool::builder().name("uh\0oh").build();
}

#[test]
#[should_panic(expected = "thread pool minimum size cannot be larger than maximum size")]
fn invalid_size_panics() {
    ThreadPool::builder().size(2..1);
}

#[test]
#[should_panic(expected = "thread pool maximum size must be non-zero")]
fn invalid_size_zero_panics() {
    ThreadPool::builder().size(0);
}

#[test]
fn execute() {
    let pool = single_thread();

    let result = pool.execute(|| 2 + 2).join();

    assert_eq!(result, 4);
}

#[test]
fn execute_future() {
    let pool = single_thread();

    let result = pool.execute_future(async { 2 + 2 }).join();

    assert_eq!(result, 4);
}

#[test]
fn task_join_timeout() {
    let pool = single_thread();

    let result = pool
        .execute(|| thread::sleep(Duration::from_millis(50)))
        .join_timeout(Duration::from_millis(10));

    assert!(result.is_err());
}

#[test]
fn futures_that_yield_are_run_concurrently() {
    let pool = single_thread();

    assert_eq!(pool.running_tasks(), 0);

    let first = pool
        .try_execute_future(Delay::new(Duration::from_millis(100)))
        .unwrap();

    // Even though there's only one worker thread, it should become idle quickly
    // and start polling for more work, because a delay future yields
    // immediately and doesn't wake for a while.
    thread::sleep(Duration::from_millis(10));

    assert_eq!(pool.running_tasks(), 1);

    let second = pool
        .try_execute_future(Delay::new(Duration::from_millis(100)))
        .unwrap();

    thread::sleep(Duration::from_millis(10));

    // Now both tasks are running, but there's still only 1 worker thread!
    assert_eq!(pool.running_tasks(), 2);
    assert_eq!(pool.threads(), 1);

    first.join();
    second.join();

    // Both tasks completed.
    assert_eq!(pool.completed_tasks(), 2);
}

#[test]
fn try_execute_under_core_count() {
    let pool = ThreadPool::builder().size(1).build();

    // Give some time for thread to start...
    thread::sleep(Duration::from_millis(50));
    assert_eq!(pool.threads(), 1);

    assert!(pool.try_execute(|| 2 + 2).is_some());
}

#[test]
fn try_execute_over_core_count() {
    let pool = ThreadPool::builder().size(0..1).build();

    assert!(pool.try_execute(|| 2 + 2).is_some());
}

#[test]
fn try_execute_over_limit() {
    let pool = ThreadPool::builder().size(0..1).queue_limit(0).build();

    assert!(pool.try_execute(|| 2 + 2).is_some());
    assert!(pool.try_execute(|| 2 + 2).is_none());
}

#[test]
fn name() {
    let pool = ThreadPool::builder().name("foo").build();

    let name = pool
        .execute(|| thread::current().name().unwrap().to_owned())
        .join();

    assert_eq!(name, "foo");
}

#[test]
#[should_panic(expected = "oh no!")]
fn panic_propagates_to_task() {
    let pool = single_thread();

    pool.execute(|| panic!("oh no!")).join();
}

#[test]
fn panic_count() {
    let pool = single_thread();
    assert_eq!(pool.panicked_tasks(), 0);

    let task = pool.execute(|| panic!("oh no!"));
    let _ = catch_unwind(move || {
        task.join();
    });

    assert_eq!(pool.panicked_tasks(), 1);
}

#[test]
fn thread_count() {
    let pool = ThreadPool::builder().size(0..1).build();

    assert_eq!(pool.threads(), 0);

    pool.execute(|| 2 + 2).join();
    assert_eq!(pool.threads(), 1);

    let pool_with_starting_threads = ThreadPool::builder().size(1).build();

    // Give some time for thread to start...
    thread::sleep(Duration::from_millis(50));
    assert_eq!(pool_with_starting_threads.threads(), 1);
}

#[test]
fn idle_shutdown() {
    let pool = ThreadPool::builder()
        .size(0..1)
        .idle_timeout(Duration::from_millis(100))
        .build();
    assert_eq!(pool.threads(), 0, "pool starts out empty");

    pool.execute(|| 2 + 2).join();
    assert_eq!(pool.threads(), 1, "one thread was added");

    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        pool.threads(),
        0,
        "thread became idle and terminated after timeout"
    );
}

#[test]
fn tasks_completed() {
    let pool = ThreadPool::default();
    assert_eq!(pool.completed_tasks(), 0);

    pool.execute(|| 2 + 2).join();
    assert_eq!(pool.completed_tasks(), 1);

    pool.execute(|| 2 + 2).join();
    assert_eq!(pool.completed_tasks(), 2);
}

#[test]
fn join() {
    // Just a dumb test to make sure join doesn't do anything strange.
    ThreadPool::default().join();
}

#[test]
fn join_timeout_expiring() {
    let pool = ThreadPool::builder().size(1).build();
    assert_eq!(pool.threads(), 1);

    // Schedule a slow task on the only thread. We have to keep the task
    // around, because dropping it could cancel the task.
    let _task = pool.execute(|| thread::sleep(Duration::from_millis(50)));

    // Joining should time out since there's one task still running longer
    // than our join timeout.
    assert!(!pool.join_timeout(Duration::from_millis(10)));
}
