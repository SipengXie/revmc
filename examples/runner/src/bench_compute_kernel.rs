mod bench_common;

use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, Criterion};

const COMPUTE_KERNEL_BYTECODE: &[u8] = &hex_literal::hex!(
    "608060405234801561000f575f5ffd5b5060043610610029575f3560e01c8063e56af0f71461002d575b5f5ffd5b610047600480360381019061004291906101be565b61005d565b6040516100549190610214565b60405180910390f35b5f5f5f90505b8181101561011f578460405180604001604052808581526020018481526020018381525090505f5b60208110156100bb57866001600160a01b03168283815181106100ac576100ac61022d565b60200260200101516040516020016100c0929190610255565b6040516020818303038152906040528051906020012090508080600101915050610087565b505f5b6020811015610108578282815181106100ea576100ea61022d565b60200260200101516040518363ffffffff1660e01b815260040161011d929190610255565b6020604051808303815f875af1158015610139573d5f5f3e3d5ffd5b50505050505080806001019150506100c5565b5080806101519190610286565b91505080806101619190610286565b925050808061017191906102b9565b9350505050509392505050565b5f5ffd5b5f819050919050565b61019d8161018b565b81146101a7575f5ffd5b50565b5f813590506101b881610194565b92915050565b5f5f5f606084860312156101d5576101d4610187565b5b5f6101e2868287016101aa565b93505060206101f3868287016101aa565b9250506040610204868287016101aa565b9150509250925092565b61020f8161018b565b82525050565b5f6020820190506102285f830184610206565b92915050565b634e487b7160e01b5f52603260045260245ffd5b5f6040820190506102685f830185610206565b6102756020830184610206565b9392505050565b5f6102808261018b565b915061028b8361018b565b92508282019050808211156102a3576102a261022d565b5b92915050565b5f6102b38261018b565b91506102be8361018b565b92508282026102cc8161018b565b915082820484148315176102e3576102e261022d565b5b509291505056fea264697066735822122024fe6014bf0f37f8c6c65eb6350705d1af6efc541cd527ef4bdd88ef2264fd2d64736f6c634300081e0033"
);

fn bench_compute_kernel(c: &mut Criterion) {
    let calldata = [
        0xe5, 0x6a, 0xf0, 0xf7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x04, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x00, 0x40,
    ];

    let fixture = bench_common::Fixture::from_bytecode(COMPUTE_KERNEL_BYTECODE, &calldata)
        .expect("failed to build compute fixture");

    let mut group = c.benchmark_group("compute_kernel");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(10));

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

criterion_group!(benches, bench_compute_kernel);
criterion_main!(benches);
