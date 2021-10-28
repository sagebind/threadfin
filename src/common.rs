use crate::{Builder, CommonAlreadyInitializedError, ThreadPool};
use once_cell::sync::OnceCell;

static COMMON: OnceCell<ThreadPool> = OnceCell::new();

/// Get a shared reference to a common thread pool for the entire process.
///
/// # Examples
///
/// ```
/// let result = threadfin::common().execute(|| 2 + 2).join();
///
/// assert_eq!(result, 4);
/// ```
pub fn common() -> &'static ThreadPool {
    COMMON.get_or_init(|| common_builder().build())
}

/// Configure the common thread pool.
///
/// This should be done near the start of your program before any other code
/// uses the common pool, as this function will return an error if the common
/// pool has already been initialized.
///
/// Only programs should use this function! Libraries should not use this
/// function and instead allow the running program to configure the common pool.
/// If you need a customized pool in a library then you should use a separate
/// pool instance.
///
/// # Examples
///
/// ```
/// threadfin::configure_common(|builder| builder
///     .size(3)
///     .queue_limit(1024))
///     .unwrap();
///
/// assert_eq!(threadfin::common().threads(), 3);
/// ```
pub fn configure_common<F>(f: F) -> Result<(), CommonAlreadyInitializedError>
where
    F: FnOnce(Builder) -> Builder,
{
    let mut was_initialized = true;

    COMMON.get_or_init(|| {
        was_initialized = false;
        f(common_builder()).build()
    });

    if was_initialized {
        Err(CommonAlreadyInitializedError::new())
    } else {
        Ok(())
    }
}

fn common_builder() -> Builder {
    Builder::default().name("common-pool")
}
