A thread pool designed for background value computation.

## Examples

```rust
use threadfin::ThreadPool;

let pool = ThreadPool::builder().size(8).build();
```
