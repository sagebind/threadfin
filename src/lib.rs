//! A thread pool for running multiple tasks on a configurable group of threads.
//!
//! Extra features:
//!
//! - Dynamic pool size based on load
//! - Support for async tasks
//! - Tasks return a handle which can be joined or awaited for the return value
//! - Optional common process-wide thread pool
//!
//! ## Async support
//!
//! Threadfin supports asynchronous usage via futures, and allows you to mix and
//! match both synchronous and asynchronous tasks within a single thread pool.
//!
//! ## Examples
//!
//! ```
//! // Create a new pool.
//! let pool = threadfin::builder().size(8).build();
//!
//! // Schedule some work.
//! let compute_task = pool.execute(|| {
//!     // Some expensive computation
//!     2 + 2
//! });
//!
//! // Do something in the meantime.
//! println!("Waiting for result...");
//!
//! // Wait for the task to complete and get the result.
//! let sum = compute_task.join();
//! println!("Sum: 2 + 2 = {}", sum);
//! ```

mod common;
mod error;
mod pool;
mod task;
mod wakers;
mod worker;

pub use crate::{
    common::*,
    error::*,
    pool::{Builder, PerCore, SizeConstraint, ThreadPool},
    task::Task,
};

/// Get a builder for creating a customized thread pool.
///
/// A shorthand for [`ThreadPool::builder`].
#[inline]
pub fn builder() -> Builder {
    ThreadPool::builder()
}
