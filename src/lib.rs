#![doc = include_str!("../README.md")]

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
