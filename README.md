A thread pool designed for background value computation.

## Examples

```rust
use skipper::ThreadPool;

let pool = ThreadPool::builder().size(8).build();
```
