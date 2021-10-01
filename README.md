# Threadfin

A thread pool for running multiple tasks on a configurable group of threads.

Extra features:

- Dynamic pool size based on load
- Support for async tasks
- Tasks return a handle which can be joined or awaited for the return value
- Optional common process-wide thread pool

## Async support

Threadfin supports asynchronous usage via futures, and allows you to mix and match both synchronous and asynchronous code with a single thread pool.

## Examples

```rust
use threadfin::ThreadPool;

let pool = ThreadPool::builder().size(8).build();
```

## Other libraries

- [threadpool](https://github.com/rust-threadpool/rust-threadpool)
- [scoped_threadpool](https://github.com/kimundi/scoped-threadpool-rs)
- [rusty_pool](https://github.com/robinfriedli/rusty_pool)
- [rayon](https://github.com/rayon-rs/rayon)

## License

Licensed under the MIT license. See the [LICENSE](LICENSE) file for details.
