//! Untimed, single-threaded interpreter replay that attributes every
//! [JUMP-HIT] (masked metadata jump that vanilla semantics would accept) to
//! its (block, tx_index, caller, to). TxBin carries no tx hash; the caller
//! can recover it on-chain from (block, index). Never touches timed paths.

#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use std::path::PathBuf;

use clap::Parser;
use op_revm::OpHaltReason;
use revm::handler::Handler;
use revm::interpreter::emse_jump_audit::take_masked_jump_hits;

use bin_common::{build_op_tx, make_evm, BenchError, BenchEvm, BinLoader};

struct PlainInterp;

impl Handler for PlainInterp {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = OpHaltReason;
}

#[derive(Parser)]
#[command(name = "jump_hit_attribution")]
struct Args {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data_base")]
    bench_dir: String,
    #[arg(long, default_value_t = 38004930)]
    start_block: u64,
    #[arg(long, default_value_t = 10000)]
    block_count: u64,
}

fn main() {
    let args = Args::parse();
    let bench_dir = PathBuf::from(&args.bench_dir);
    let _ = take_masked_jump_hits();
    let mut total_hits = 0u64;
    let mut handler = PlainInterp;

    for block in args.start_block..args.start_block + args.block_count {
        let Ok(loader) = BinLoader::new(&bench_dir, block) else { continue };
        let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
        let mut evm = make_evm(&loader, chain_id);
        for (i, tx_bin) in loader.raw_txs().iter().enumerate() {
            if tx_bin.tx_type == 0x7e {
                continue;
            }
            evm.0.ctx.tx = build_op_tx(tx_bin);
            let _ = handler.run(&mut evm);
            let hits = take_masked_jump_hits();
            if hits > 0 {
                total_hits += hits;
                println!(
                    "JUMP-HIT: block {} tx_index {} hits {} caller 0x{} to {} calldata[..8] 0x{}",
                    block,
                    i,
                    hits,
                    hex::encode(tx_bin.caller),
                    tx_bin
                        .to
                        .map(|a| format!("0x{}", hex::encode(a)))
                        .unwrap_or_else(|| "CREATE".into()),
                    hex::encode(&tx_bin.data[..tx_bin.data.len().min(8)])
                );
            }
        }
        if (block - args.start_block + 1) % 1000 == 0 {
            eprintln!("  [{}] blocks scanned, {total_hits} hits so far", block - args.start_block + 1);
        }
    }
    println!("total JUMP-HIT events: {total_hits}");
}
