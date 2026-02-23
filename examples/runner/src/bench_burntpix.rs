mod bench_common;

use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, Criterion};

const FIXTURE_RELATIVE_PATH: &str = "data/burntpix-benchmark.json";

pub fn bench_burntpix(c: &mut Criterion) {
    let fixture =
        bench_common::Fixture::load(FIXTURE_RELATIVE_PATH).expect("failed to load JSON fixture");

    let mut group = c.benchmark_group("burntpix");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));
    group.bench_function("plain_execution", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let result = fixture.run_plain().expect("plain execution failed");
                total += start.elapsed();
                black_box(result);
            }
            total
        });
    });
    group.bench_function("jit_optimized", |b| {
        b.iter_custom(|iters| {
            let mut total = Duration::ZERO;
            for _ in 0..iters {
                let start = Instant::now();
                let result = fixture.run_jit().expect("jit execution failed");
                total += start.elapsed();
                black_box(result);
            }
            total
        });
    });
    group.finish();
}

criterion_group!(benches, bench_burntpix);
criterion_main!(benches);
