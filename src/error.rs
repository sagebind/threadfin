use std::{error::Error, fmt};

/// An error returned when a task could not be executed because a thread pool
/// was full.
///
/// Contains the original task that failed to be submitted. This allows you to
/// try the submission again later or take some other action.
pub struct PoolFullError<T>(pub(crate) T);

impl<T> PoolFullError<T> {
    /// Extracts the inner task that could not be executed.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> Error for PoolFullError<T> {}

impl<T> fmt::Debug for PoolFullError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PoolFullError(..)")
    }
}

impl<T> fmt::Display for PoolFullError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("thread pool is full")
    }
}
