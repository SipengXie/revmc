//! Per-tx outcome census for one block range: success / revert / halt reason.
#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;
#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;
use bin_common::{build_op_tx, make_evm, BenchEvm, BenchError, BinLoader};
use op_revm::OpHaltReason;
use revm::context_interface::result::ExecutionResult;
use revm::handler::Handler;
use std::collections::HashMap;
use std::path::Path;

struct Plain;
impl Handler for Plain {
    type Evm = BenchEvm;
    type Error = BenchError;
    type HaltReason = OpHaltReason;
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (dir, start, count) = (&a[1], a[2].parse::<u64>().unwrap(), a[3].parse::<u64>().unwrap());
    let mut census: HashMap<String, u64> = HashMap::new();
    let mut h = Plain;
    for bn in start..start + count {
        let Ok(loader) = BinLoader::new(Path::new(dir), bn) else { continue };
        let chain_id = loader.raw_txs().first().and_then(|t| t.chain_id);
        let mut evm = make_evm(&loader, chain_id);
        for tx in loader.raw_txs() {
            if tx.tx_type == 0x7e { *census.entry("skipped:deposit".into()).or_default() += 1; continue; }
            evm.0.ctx.tx = build_op_tx(tx);
            let key = match h.run(&mut evm) {
                Ok(ExecutionResult::Success { .. }) => "success".to_string(),
                Ok(ExecutionResult::Revert { .. }) => "revert".to_string(),
                Ok(ExecutionResult::Halt { reason, .. }) => format!("halt:{reason:?}"),
                Err(e) => format!("error:{}", format!("{e:?}").chars().take(40).collect::<String>()),
            };
            *census.entry(key).or_default() += 1;
        }
    }
    let total: u64 = census.values().sum();
    let mut v: Vec<_> = census.into_iter().collect();
    v.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (k, c) in v {
        println!("{:>8} ({:5.2}%)  {k}", c, c as f64 / total as f64 * 100.0);
    }
    println!("total {total}");
}
