mod bench_common;

use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn load_hex_bytecode(relative_path: &str) -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join(relative_path);
    let hex_str = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
    let trimmed = hex_str.trim().strip_prefix("0x").unwrap_or(hex_str.trim());
    hex::decode(trimmed).expect("invalid hex in bytecode file")
}

fn bench_variant(c: &mut Criterion, group_name: &str, bytecode_path: &str) {
    let bytecode = load_hex_bytecode(bytecode_path);
    // `Benchmark()` selector
    let calldata: &[u8] = &[0x30, 0x62, 0x7b, 0x7c];

    let fixture = bench_common::Fixture::from_bytecode(&bytecode, calldata)
        .unwrap_or_else(|e| panic!("failed to build {group_name} fixture: {e}"));

    let mut group = c.benchmark_group(group_name);
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

pub fn bench_snailtracer(c: &mut Criterion) {
    bench_variant(c, "snailtracer", "data/snailtracer.rt.hex");
}

pub fn bench_snailtracer_compact(c: &mut Criterion) {
    bench_variant(c, "snailtracer_compact", "data/snailtracer-compact.hex");
}

criterion_group!(benches, bench_snailtracer, bench_snailtracer_compact);
criterion_main!(benches);
