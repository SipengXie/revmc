//! Classify all JIT-slower txs in a block by pattern and show per-pattern speedup stats.
//!
//! Usage:
//!   cargo run -p revmc-examples-runner --bin classify_slow_txs --release

#[allow(dead_code)]
#[path = "../bin_common.rs"]
mod bin_common;

use std::collections::HashMap;
use std::path::Path;
use revm::primitives::HashMap as RevmHashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use op_revm::{transaction::OpTransaction, DefaultOp};
use revm::{database::EmptyDB, handler::Handler, primitives::{Address, B256}};
use revmc::OptimizationLevel;
use revmc_builtins as _;
use revmc_context::RawEvmCompilerFn;

use bin_common::{
    build_op_cfg, build_op_tx, compile_all_contracts_with_cache, BenchEvm, BinLoader,
    JitHandler, NativeHandler, OpCtx,
};

// ── Bytecode pattern classification ─────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TxPattern {
    TinyProxy,       // ≤50B proxy (EIP-1167 or variant)
    SmallContract,   // ≤1000B, not proxy
    CreateTx,        // contract deployment
    Eoa,             // target has no code
    ShortErc20,      // approve/transferFrom on any-size contract
    General,         // everything else
}

impl TxPattern {
    fn label(&self) -> &'static str {
        match self {
            TxPattern::TinyProxy    => "tiny proxy (≤50B)",
            TxPattern::SmallContract=> "small contract (≤1000B)",
            TxPattern::CreateTx     => "CREATE (deployment)",
            TxPattern::Eoa          => "EOA / no code",
            TxPattern::ShortErc20   => "short ERC20 op (approve/transferFrom)",
            TxPattern::General      => "general contract",
        }
    }
}

fn classify(
    tx: &bin_common::TxBin,
    addr_to_hash: &HashMap<Address, B256>,
    code_values: &RevmHashMap<B256, revm::bytecode::Bytecode>,
) -> TxPattern {
    // CREATE tx
    if tx.to.is_none() {
        return TxPattern::CreateTx;
    }
    let addr = Address::from_slice(tx.to.as_ref().unwrap());

    // ERC20 approve / transferFrom selector
    let sel = if tx.data.len() >= 4 { Some(&tx.data[..4]) } else { None };
    let is_short_erc20 = matches!(sel,
        Some([0x09, 0x5e, 0xa7, 0xb3]) |  // approve(address,uint256)
        Some([0x23, 0xb8, 0x72, 0xdd])     // transferFrom(address,address,uint256)
    );

    let hash = match addr_to_hash.get(&addr) {
        None => return TxPattern::Eoa,
        Some(h) => *h,
    };
    let bytecode = match code_values.get(&hash) {
        None => return TxPattern::Eoa,
        Some(bc) => bc,
    };
    let bytes = bytecode.original_byte_slice();

    if bytes.len() <= 50 {
        return TxPattern::TinyProxy;
    }
    if is_short_erc20 {
        return TxPattern::ShortErc20;
    }
    if bytes.len() <= 1000 {
        return TxPattern::SmallContract;
    }
    TxPattern::General
}

// ── Per-tx benchmark ─────────────────────────────────────────────────────────

fn run_all_txs(
    loader: &BinLoader,
    functions: Option<&Arc<HashMap<B256, RawEvmCompilerFn>>>,
) -> Vec<Duration> {
    let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
    let cfg = build_op_cfg(chain_id);
    let dummy_tx = OpTransaction::builder().build_fill();
    let mut evm: BenchEvm = {
        let ctx = OpCtx::<EmptyDB>::op()
            .with_cfg(cfg)
            .with_db(loader.build_cache_db())
            .with_tx(dummy_tx);
        op_revm::OpEvm::new(ctx, ())
    };
    let mut per_tx = Vec::with_capacity(loader.tx_count());
    for tx_bin in loader.raw_txs() {
        if tx_bin.tx_type == 0x7e {
            per_tx.push(Duration::ZERO);
            continue;
        }
        evm.0.ctx.tx = build_op_tx(tx_bin);
        let t0 = Instant::now();
        if let Some(fns) = functions {
            let mut h = JitHandler { functions: fns.clone() };
            let _ = h.run(&mut evm);
        } else {
            let mut h = NativeHandler;
            let _ = h.run(&mut evm);
        }
        per_tx.push(t0.elapsed());
    }
    per_tx
}

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "classify_slow_txs")]
struct Args {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data")]
    dir: String,
    #[arg(long, default_value_t = 38004930)]
    block: u64,
    #[arg(long, default_value = "./jit_cache")]
    cache_dir: String,
    #[arg(long, default_value_t = 7)]
    rounds: usize,
    #[arg(long, default_value_t = 2)]
    warmup: usize,
}

fn main() {
    let args = Args::parse();
    let loader = BinLoader::new(Path::new(&args.dir), args.block).expect("load block");

    // Build address → code hash lookup
    let addr_to_hash: HashMap<Address, B256> = loader.snapshot.accounts.iter()
        .filter_map(|(addr, acc)| {
            acc.info.as_ref().and_then(|i| {
                if i.code_hash == revm::primitives::KECCAK_EMPTY { None }
                else { Some((*addr, i.code_hash)) }
            })
        })
        .collect();

    // Load JIT cache
    let all_codes: HashMap<B256, _> = loader.code_values().iter()
        .map(|(h, bc)| (*h, bc.clone()))
        .collect();
    let compiled = compile_all_contracts_with_cache(
        &all_codes, OptimizationLevel::Aggressive, Path::new(&args.cache_dir),
    );
    eprintln!("  {} JIT functions loaded", compiled.functions.len());

    // Benchmark
    let total_rounds = args.warmup + args.rounds;
    let n = loader.tx_count();
    let mut native_samples: Vec<Vec<u64>> = Vec::new();
    let mut jit_samples: Vec<Vec<u64>> = Vec::new();

    for round in 0..total_rounds {
        let native = run_all_txs(&loader, None);
        let jit    = run_all_txs(&loader, Some(&compiled.functions));
        if round >= args.warmup {
            native_samples.push(native.iter().map(|d| d.as_nanos() as u64).collect());
            jit_samples.push(jit.iter().map(|d| d.as_nanos() as u64).collect());
        }
    }

    // Median per tx
    let r = args.rounds;
    let mut results: Vec<(usize, f64, f64, f64, TxPattern)> = Vec::new();
    for i in 0..n {
        let tx = &loader.raw_txs()[i];
        if tx.tx_type == 0x7e { continue; }

        let mut ns: Vec<u64> = (0..r).map(|rr| native_samples[rr][i]).collect();
        let mut js: Vec<u64> = (0..r).map(|rr| jit_samples[rr][i]).collect();
        ns.sort_unstable(); js.sort_unstable();
        let n_us = ns[r / 2] as f64 / 1000.0;
        let j_us = js[r / 2] as f64 / 1000.0;
        if n_us < 0.5 { continue; }

        let sp = if j_us > 0.0 { n_us / j_us } else { 0.0 };
        let pat = classify(tx, &addr_to_hash, loader.code_values());
        results.push((i, n_us, j_us, sp, pat));
    }

    // Split into slower / faster
    let mut slower: Vec<_> = results.iter().filter(|r| r.3 < 1.0).collect();
    slower.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap());

    println!("\n=== Block {} — {} JIT-slower txs ===\n", args.block, slower.len());

    // Per-pattern aggregate
    let mut by_pat: HashMap<&TxPattern, Vec<f64>> = HashMap::new();
    for row in &slower {
        by_pat.entry(&row.4).or_default().push(row.3);
    }
    let mut pat_list: Vec<_> = by_pat.iter().collect();
    pat_list.sort_by_key(|(_, v)| (v.iter().cloned().fold(f64::INFINITY, f64::min) * 1000.0) as i64);

    println!("{:<38}  {:>5}  {:>7}  {:>7}  {:>7}",
        "Pattern", "count", "min", "median", "max");
    println!("{}", "-".repeat(72));
    for (pat, speedups) in &pat_list {
        let mut sp: Vec<f64> = speedups.to_vec();
        sp.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let min    = sp.first().copied().unwrap_or(0.0);
        let median = sp[sp.len() / 2];
        let max    = sp.last().copied().unwrap_or(0.0);
        println!("{:<38}  {:>5}  {:>6.2}x  {:>6.2}x  {:>6.2}x",
            pat.label(), sp.len(), min, median, max);
    }

    // Full list of all slower txs
    println!("\n--- All JIT-slower txs ---");
    println!("  {:>6}  {:>9}  {:>9}  {:>7}  {:<38}  to",
        "tx_idx", "native_µs", "jit_µs", "speedup", "pattern");
    for &(idx, n_us, j_us, sp, ref pat) in &slower {
        let to = loader.raw_txs()[*idx].to
            .map(|a| format!("0x{}", hex::encode(&a[16..])))
            .unwrap_or_else(|| "CREATE".to_string());
        println!("  {:>6}  {:>9.1}  {:>9.1}  {:>6.2}x  {:<38}  {}",
            idx, n_us, j_us, sp, pat.label(), to);
    }
}
