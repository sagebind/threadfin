# Threadfin

A thread pool for running multiple tasks on a configurable group of threads.

Extra features:

- Optional common process-wide thread pool
- Support for async tasks
- Dynamic pool size based on load
- CPU core pinning

## Examples

```rust
use threadfin::ThreadPool;

let pool = ThreadPool::builder().size(8).build();
```

## Other libraries

- [threadpool](https://github.com/rust-threadpool/rust-threadpool): More popular but less customizable.
- [rayon](https://github.com/rayon-rs/rayon): Designed for data parallelization, operates at a higher abstraction layer.

## License

Licensed under the MIT license. See the [LICENSE](LICENSE) file for details.
