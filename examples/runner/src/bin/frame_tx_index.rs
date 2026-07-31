//! Frame-level representative-transaction index.
//!
//! `tx_index.rs` keys the index on `tx.to`, so a contract reached only
//! through internal calls (CALL/DELEGATECALL/STATICCALL from another
//! contract) never gets a representative transaction — the ablation then
//! reports `[exec-skip]` for its family. This tool replays the corpus with
//! the interpreter and records, for every code hash that executes in ANY
//! frame, the first `(block, tx_index)` at which it appeared.
//!
//! Output is bincode `HashMap<B256, (u64, usize)>` — the same format
//! `build_tx_index_cached` produces, so consumers only need a path.
//!
//! Untimed and single-threaded by construction (the index must be
//! deterministic: "first occurrence" depends on block/tx order).

#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use op_revm::OpHaltReason;
use revm::handler::{EvmTr, FrameResult, Handler, ItemOrResult};
use revm::primitives::B256;

use bin_common::{build_op_tx, make_evm, BenchError, BenchEvm, BinLoader};

#[derive(Parser)]
#[command(name = "frame_tx_index", about = "Build a frame-level code_hash -> (block, tx_index) index")]
struct Args {
    #[arg(long, default_value = "/home/ubuntu/sipeng/bench_data_base")]
    bench_dir: String,
    #[arg(long, default_value_t = 38004930)]
    start_block: u64,
    #[arg(long, default_value_t = 10000)]
    block_count: u64,
    #[arg(long)]
    out: String,
}

struct IndexCollector {
    index: HashMap<B256, (u64, usize)>,
    block: u64,
    tx_index: usize,
    frames: u64,
}

impl IndexCollector {
    fn note(&mut self, evm: &mut BenchEvm) {
        let frame = evm.0.frame_stack.get();
        self.frames += 1;
        if !bin_common::should_lookup_jit(
            frame.data.is_create(),
            frame.interpreter.input.bytecode_address,
            frame.interpreter.bytecode.is_empty(),
        ) {
            return;
        }
        let hash = frame.interpreter.bytecode.get_or_calculate_hash();
        self.index.entry(hash).or_insert((self.block, self.tx_index));
    }
}

impl Handler for IndexCollector {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = OpHaltReason;

    fn run_exec_loop(
        &mut self,
        evm: &mut Self::Evm,
        first_frame_input: revm::interpreter::interpreter_action::FrameInit,
    ) -> Result<FrameResult, Self::Error> {
        let res = evm.frame_init(first_frame_input)?;
        if let ItemOrResult::Result(frame_result) = res {
            return Ok(frame_result);
        }
        self.note(evm);
        loop {
            let call_or_result = evm.frame_run()?;
            let result = match call_or_result {
                ItemOrResult::Item(init) => match evm.frame_init(init)? {
                    ItemOrResult::Item(_) => {
                        self.note(evm);
                        continue;
                    }
                    ItemOrResult::Result(result) => result,
                },
                ItemOrResult::Result(result) => result,
            };
            if let Some(result) = evm.frame_return_result(result)? {
                return Ok(result);
            }
        }
    }
}

fn main() {
    let args = Args::parse();
    let bench_dir = PathBuf::from(&args.bench_dir);
    let mut collector =
        IndexCollector { index: HashMap::new(), block: 0, tx_index: 0, frames: 0 };
    let t0 = Instant::now();
    let mut blocks = 0u64;

    for block in args.start_block..args.start_block + args.block_count {
        let Ok(loader) = BinLoader::new(&bench_dir, block) else { continue };
        let chain_id = loader.raw_txs().first().and_then(|tx| tx.chain_id);
        let mut evm = make_evm(&loader, chain_id);
        collector.block = block;
        for (i, tx_bin) in loader.raw_txs().iter().enumerate() {
            // Deposits are skipped by every exec driver; a rep tx must be
            // replayable under the same convention.
            if tx_bin.tx_type == 0x7e {
                continue;
            }
            collector.tx_index = i;
            evm.0.ctx.tx = build_op_tx(tx_bin);
            let _ = collector.run(&mut evm);
        }
        blocks += 1;
        if blocks % 1000 == 0 {
            eprintln!(
                "  [{blocks}] blocks, {} hashes indexed, {:.0}s",
                collector.index.len(),
                t0.elapsed().as_secs_f64()
            );
        }
    }

    println!("blocks replayed:   {blocks}");
    println!("frames seen:       {}", collector.frames);
    println!("code hashes indexed: {}", collector.index.len());
    println!("wall: {:.1}s", t0.elapsed().as_secs_f64());

    let path = PathBuf::from(&args.out);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let data = bincode::serialize(&collector.index).expect("serialize index");
    std::fs::write(&path, &data).expect("write index");
    eprintln!("wrote {} ({:.1} MB)", path.display(), data.len() as f64 / 1048576.0);
}
