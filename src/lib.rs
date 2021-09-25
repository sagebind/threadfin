#![doc = include_str!("../README.md")]

mod error;
mod pool;
mod task;
mod wakers;
mod worker;

pub use crate::{
    error::PoolFullError,
    pool::{Builder, SizeConstraint, ThreadPool},
    task::Task,
};

/// Get a shared reference to a common thread pool for the entire process.
///
/// # Examples
///
/// ```
/// let result = threadfin::common_pool().execute(|| 2 + 2).join();
///
/// assert_eq!(result, 4);
/// ```
#[cfg(feature = "common")]
pub fn common_pool() -> &'static ThreadPool {
    use once_cell::sync::OnceCell;

    static COMMON: OnceCell<ThreadPool> = OnceCell::new();

    COMMON.get_or_init(ThreadPool::default)
}
