#![doc = include_str!("../README.md")]

mod pool;
mod task;
mod wakers;
mod worker;

pub use crate::{
    pool::{SizeConstraint, ThreadPool, ThreadPoolBuilder},
    task::Task,
};
