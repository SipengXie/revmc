//! Stage A — SkelOT paper dataset scanner.

#[path = "../bin_common.rs"]
#[allow(dead_code)]
mod bin_common;

#[path = "../skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use clap::Parser;

#[derive(Parser)]
#[command(name = "skelot_dataset_scan")]
struct Args {
    #[arg(long)]
    bench_dir: String,
    #[arg(long, default_value_t = 38004930)]
    start_block: u64,
    #[arg(long, default_value_t = 10000)]
    block_count: u64,
    #[arg(long, default_value_t = 1000)]
    window_size: u64,
    #[arg(long)]
    out_dir: String,
}

fn main() {
    unimplemented!("Stage A scanner — implemented in Task 1.2");
}

#[cfg(test)]
mod tests {
    #[test]
    fn bucket_boundaries_use_raw_bytecode_length() {
        use crate::skelot_common::extract_opcode_skeleton_with_counts;
        // A 120-byte bytecode with a 20-byte PUSH20 payload should land in
        // the `small` bucket (100..500B), because we bucket by raw length.
        let bc = std::iter::repeat(0x00u8)
            .take(80)
            .chain(std::iter::repeat(0x73u8).take(1))
            .chain(std::iter::repeat(0xFFu8).take(20))
            .chain(std::iter::repeat(0x00u8).take(19))
            .collect::<Vec<u8>>();
        assert_eq!(bc.len(), 120);
        let bucket = bucket_for_raw_len(bc.len());
        assert_eq!(bucket, "small");
        let (skel, _, imm) = extract_opcode_skeleton_with_counts(&bc);
        assert_eq!(imm, 20);
        assert!(skel.len() < bc.len());
    }

    fn bucket_for_raw_len(len: usize) -> &'static str {
        match len {
            0..=99 => "tiny",
            100..=499 => "small",
            500..=4_999 => "medium",
            5_000..=14_999 => "large",
            _ => "huge",
        }
    }
}
