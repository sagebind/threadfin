# Threadfin

A thread pool for running multiple tasks on a configurable group of threads.

[![Crates.io](https://img.shields.io/crates/v/threadfin.svg)](https://crates.io/crates/threadfin)
[![Documentation](https://docs.rs/threadfin/badge.svg)](https://docs.rs/threadfin)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Minimum supported Rust version](https://img.shields.io/badge/rustc-1.51+-yellow.svg)](#minimum-supported-rust-version)
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
// Create a new pool.
let pool = threadfin::builder().size(8).build();

// Schedule some work.
let compute_task = pool.execute(|| {
    // Some expensive computation
    2 + 2
});

// Do something in the meantime.
println!("Waiting for result...");

// Wait for the task to complete and get the result.
let sum = compute_task.join();
println!("Sum: 2 + 2 = {}", sum);
```

## Installation

Install via Cargo by adding to your Cargo.toml file:

```toml
[dependencies]
threadfin = "0.1"
```

### Minimum supported Rust version

The minimum supported Rust version (or MSRV) for Threadfin is stable Rust 1.46 or greater, meaning we only guarantee that Threadfin will compile if you use a rustc version of at least 1.46. It might compile with older versions but that could change at any time.

This version is explicitly tested in CI and may only be bumped in new minor versions. Any changes to the supported minimum version will be called out in the release notes.

## Other libraries

- [threadpool](https://github.com/rust-threadpool/rust-threadpool)
- [scoped_threadpool](https://github.com/kimundi/scoped-threadpool-rs)
- [rusty_pool](https://github.com/robinfriedli/rusty_pool)
- [rayon](https://github.com/rayon-rs/rayon)

## Sponsors

Special thanks to sponsors of my open-source work!

<!-- sponsors --><a href="https://github.com/da-moon"><img src="https://github.com/da-moon.png" width="60px" alt="da-moon" /></a><!-- sponsors -->

## License

Licensed under the MIT license. See the [LICENSE](LICENSE) file for details.
