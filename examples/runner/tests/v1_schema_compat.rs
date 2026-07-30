//! Pre-flight: confirm bench_data_*_v1/states/*.bin (BytecodeV86 schema) decodes
//! via the BytecodeV86Mirror -> Bytecode path used by all xchain analyses.

#[path = "../src/skelot_common.rs"]
#[allow(dead_code)]
mod skelot_common;

use std::path::Path;

#[test]
fn eth_v1_state_decodes() {
    let path = Path::new("/home/ubuntu/sipeng/bench_data_eth/states/23000000.bin");
    let snap = skelot_common::load_state_v1(path).expect("decode eth v1");
    assert_eq!(snap.block_number, 23_000_000);
    assert!(!snap.codes.is_empty(), "expected >=1 code in eth v1 state");
    let mut total = 0usize;
    let mut eip7702 = 0usize;
    skelot_common::iter_bytecode_raw(&snap, |_, _| total += 1);
    for code in snap.codes.values() {
        if matches!(code, skelot_common::BytecodeV86Mirror::Eip7702 { .. }) {
            eip7702 += 1;
        }
    }
    eprintln!("eth v1 23000000: {} entries ({} eip7702)", total, eip7702);
}

#[test]
fn bsc_v1_state_decodes() {
    let path = Path::new("/home/ubuntu/sipeng/bench_data_bsc/states/95897884.bin");
    let snap = skelot_common::load_state_v1(path).expect("decode bsc v1");
    assert_eq!(snap.block_number, 95_897_884);
    assert!(!snap.codes.is_empty(), "expected >=1 code in bsc v1 state");
}

#[test]
fn manifest_reports_v1_schema() {
    let m = skelot_common::read_bench_manifest(Path::new("/home/ubuntu/sipeng/bench_data_eth"))
        .expect("eth manifest");
    assert_eq!(m.schema_version, 1);
    let m = skelot_common::read_bench_manifest(Path::new("/home/ubuntu/sipeng/bench_data_bsc"))
        .expect("bsc manifest");
    assert_eq!(m.schema_version, 1);
}
