# Threadfin

A thread pool for running multiple tasks on a configurable group of threads.

[![Crates.io](https://img.shields.io/crates/v/threadfin.svg)](https://crates.io/crates/threadfin)
[![Documentation](https://docs.rs/threadfin/badge.svg)][documentation]
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Build](https://github.com/sagebind/threadfin/workflows/ci/badge.svg)](https://github.com/sagebind/threadfin/actions)

Extra features:

- Dynamic pool size based on load
- Support for async tasks
- Tasks return a handle which can be joined or awaited for the return value
- Optional common process-wide thread pool

## Async support

Threadfin supports asynchronous usage via futures, and allows you to mix and match both synchronous and asynchronous tasks within a single thread pool.

## Examples

```rust
let pool = threadfin::builder().size(8).build();
```

## Other libraries

- [threadpool](https://github.com/rust-threadpool/rust-threadpool)
- [scoped_threadpool](https://github.com/kimundi/scoped-threadpool-rs)
- [rusty_pool](https://github.com/robinfriedli/rusty_pool)
- [rayon](https://github.com/rayon-rs/rayon)

## Sponsors

Extra thanks to those who sponsor my open-source work:

<!-- sponsors --><a href="https://github.com/da-moon"><img src="https://github.com/da-moon.png" width="60px" alt="" /></a><!-- sponsors -->

## License

Licensed under the MIT license. See the [LICENSE](LICENSE) file for details.
