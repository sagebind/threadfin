use criterion::*;

fn criterion_benchmark(c: &mut Criterion) {
    let threads = num_cpus::get().max(1);

    let tasks = 1000;

    let mut group = c.benchmark_group("pool");
    group.sample_size(10);

    group.bench_function("threadfin", |b| {
        b.iter_batched(
            || threadfin::ThreadPool::builder().size(threads).build(),
            |pool| {
                for _ in 0..tasks {
                    pool.execute(|| {
                        let _ = black_box(8 + 9);
                    });
                }

                pool.join();
            },
            BatchSize::LargeInput,
        );
    });

    group.bench_function("threadpool", |b| {
        b.iter_batched(
            || threadpool::ThreadPool::new(threads),
            |pool| {
                for _ in 0..tasks {
                    pool.execute(|| {
                        let _ = black_box(8 + 9);
                    });
                }

                pool.join();
            },
            BatchSize::LargeInput,
        );
    });

    group.bench_function("rusty_pool", |b| {
        b.iter_batched(
            || rusty_pool::ThreadPool::new(threads, threads, std::time::Duration::ZERO),
            |pool| {
                for _ in 0..tasks {
                    pool.execute(|| {
                        let _ = black_box(8 + 9);
                    });
                }

                pool.shutdown_join();
            },
            BatchSize::LargeInput,
        );
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
