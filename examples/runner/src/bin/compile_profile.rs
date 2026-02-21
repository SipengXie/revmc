//! Profile compilation phases: translate vs write_object vs link.
//! Compiles a small sample of contracts with per-phase timing.

use std::path::Path;
use std::time::Instant;

use revm::{
    bytecode::Bytecode,
    primitives::{Address, B256, HashMap as RevmHashMap, U256},
    state::AccountInfo,
};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use serde::Deserialize;

use op_revm::OpSpecId;
use revm::primitives::hardfork::SpecId;

const OP_SPEC: OpSpecId = OpSpecId::ISTHMUS;
const ETH_SPEC: SpecId = OP_SPEC.into_eth_spec();

#[derive(Deserialize)]
struct AccountSnapshot {
    #[allow(dead_code)]
    info: Option<AccountInfo>,
    #[allow(dead_code)]
    storage: RevmHashMap<U256, U256>,
}

#[derive(Deserialize)]
struct CacheSnapshot {
    #[allow(dead_code)]
    block_number: u64,
    #[allow(dead_code)]
    has_state_clear: bool,
    #[allow(dead_code)]
    accounts: RevmHashMap<Address, AccountSnapshot>,
    codes: RevmHashMap<B256, Bytecode>,
}

fn main() {
    let bench_dir = Path::new("/home/ubuntu/sipeng/bench_data");
    let cache_dir = Path::new("/tmp/compile_profile_cache");
    std::fs::create_dir_all(cache_dir).ok();
    let opt = OptimizationLevel::Aggressive;

    // Load contracts from first block
    let data = std::fs::read(bench_dir.join("states/38004930.bin")).unwrap();
    let snapshot: CacheSnapshot = bincode::deserialize(&data).unwrap();

    let mut contracts: Vec<(B256, Bytecode)> = snapshot
        .codes
        .into_iter()
        .filter(|(_, bc)| !bc.is_empty())
        .collect();
    contracts.sort_by_key(|(_, bc)| bc.original_byte_slice().len());

    // Sample: 5 small, 5 medium, 5 large
    let n = contracts.len();
    let sample_indices = [
        // small
        0, 1, 2, 3, 4,
        // medium
        n / 2 - 2, n / 2 - 1, n / 2, n / 2 + 1, n / 2 + 2,
        // large
        n - 5, n - 4, n - 3, n - 2, n - 1,
    ];

    println!(
        "{:<8} {:>8} {:>10} {:>10} {:>10} {:>10}",
        "idx", "size", "translate", "write_obj", "link", "total"
    );
    println!("{}", "-".repeat(66));

    let mut total_translate = 0.0f64;
    let mut total_write = 0.0f64;
    let mut total_link = 0.0f64;
    let mut count = 0;

    for &idx in &sample_indices {
        let (hash, bytecode) = &contracts[idx];
        let size = bytecode.original_byte_slice().len();
        let hash_hex = hex::encode(hash);
        let spec_tag = format!("{ETH_SPEC:?}").to_lowercase();
        let key = format!("{hash_hex}__{spec_tag}__o3");
        let symbol = format!("c_{hash_hex}");
        let object = cache_dir.join(format!("{key}.o"));
        let library = cache_dir.join(format!("{key}.so"));

        // Clean up any previous artifacts
        std::fs::remove_file(&object).ok();
        std::fs::remove_file(&library).ok();

        // Phase 1: Create backend + translate
        let t0 = Instant::now();
        let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
        let backend = EvmLlvmBackend::new(context, true, opt).unwrap();
        let mut compiler = EvmCompiler::new(backend);
        compiler
            .translate(&symbol, bytecode.original_byte_slice(), ETH_SPEC)
            .unwrap();
        let translate_dur = t0.elapsed();

        // Phase 2: Write object file
        let t1 = Instant::now();
        compiler.write_object_to_file(&object).unwrap();
        let write_dur = t1.elapsed();

        // Phase 3: Link to .so
        let t2 = Instant::now();
        revmc::Linker::new().link(&library, [&object]).unwrap();
        let link_dur = t2.elapsed();

        let total_dur = translate_dur + write_dur + link_dur;

        println!(
            "{:<8} {:>8} {:>9.1}ms {:>9.1}ms {:>9.1}ms {:>9.1}ms",
            idx,
            size,
            translate_dur.as_secs_f64() * 1000.0,
            write_dur.as_secs_f64() * 1000.0,
            link_dur.as_secs_f64() * 1000.0,
            total_dur.as_secs_f64() * 1000.0,
        );

        total_translate += translate_dur.as_secs_f64();
        total_write += write_dur.as_secs_f64();
        total_link += link_dur.as_secs_f64();
        count += 1;
    }

    let grand_total = total_translate + total_write + total_link;
    println!("{}", "-".repeat(66));
    println!(
        "TOTAL ({count} contracts): translate={:.1}s ({:.0}%), write={:.1}s ({:.0}%), link={:.1}s ({:.0}%)",
        total_translate,
        total_translate / grand_total * 100.0,
        total_write,
        total_write / grand_total * 100.0,
        total_link,
        total_link / grand_total * 100.0,
    );
    println!("Grand total: {:.1}s", grand_total);

    // Also time pure JIT (no AOT) for comparison
    println!("\n--- JIT-only (no disk I/O) ---");
    let mut jit_total = 0.0f64;
    for &idx in &sample_indices {
        let (hash, bytecode) = &contracts[idx];
        let size = bytecode.original_byte_slice().len();
        let hash_hex = hex::encode(hash);
        let symbol = format!("c_{hash_hex}");

        let t0 = Instant::now();
        let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
        let backend = EvmLlvmBackend::new(context, false, opt).unwrap();
        let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
            Box::leak(Box::new(EvmCompiler::new(backend)));
        let func_id = compiler
            .translate(&symbol, bytecode.original_byte_slice(), ETH_SPEC)
            .unwrap();
        let _fn_ptr = unsafe { compiler.jit_function(func_id).unwrap() };
        let dur = t0.elapsed();

        println!(
            "  idx={:<4} size={:>6} jit={:.1}ms",
            idx,
            size,
            dur.as_secs_f64() * 1000.0,
        );
        jit_total += dur.as_secs_f64();
    }
    println!("JIT total ({count} contracts): {:.1}s", jit_total);
    println!(
        "AOT/JIT ratio: {:.2}x",
        grand_total / jit_total
    );

    // Cleanup
    std::fs::remove_dir_all(cache_dir).ok();
}
