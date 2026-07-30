//! Support types for `skeleton_bench`: exec-sanity JSON schema, the
//! skeleton registry, and the skeleton `.so` cache. Split out of
//! `skeleton_bench.rs`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use revm::primitives::B256;
use revmc::skeleton::{PushClassification, SkeletonVariance};
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use revmc_context::RawEvmCompilerFn;
use serde::{Deserialize, Serialize};


// ── JSON output schema ──────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct PerTxRecord {
    pub success: bool,
    pub gas: u64,
}

#[derive(Serialize)]
pub struct ModeRecord {
    pub rounds_ms: Vec<f64>,
    pub mean_ms: f64,
    pub std_ms: f64,
    pub per_tx: Vec<PerTxRecord>,
}

#[derive(Serialize)]
pub struct PairMatch {
    pub matches: usize,
    pub total: usize,
    pub mismatches: usize,
}

#[derive(Serialize)]
pub struct NoiseFloor {
    pub mean_ms: f64,
    pub std_ms: f64,
    pub cv: f64,
}

#[derive(Serialize)]
pub struct ExecSanity {
    pub block: u64,
    pub rounds: usize,
    pub warmup: usize,
    pub alpha: f64,
    pub noise_floor: NoiseFloor,
    pub modes: std::collections::HashMap<String, ModeRecord>,
    pub pairwise_match: std::collections::HashMap<String, PairMatch>,
    pub gas_totals: std::collections::HashMap<String, u64>,
    pub welch_per_hash_vs_native: (f64, f64),
    pub welch_skelot_vs_native: (f64, f64),
    pub welch_skelot_vs_per_hash: (f64, f64),
}

pub fn build_mode(rounds: &[f64], per_tx: &[(bool, u64)]) -> ModeRecord {
    let mean = rounds.iter().sum::<f64>() / rounds.len() as f64;
    let var = rounds.iter().map(|v| (v - mean).powi(2)).sum::<f64>()
        / (rounds.len() as f64 - 1.0).max(1.0);
    ModeRecord {
        rounds_ms: rounds.to_vec(),
        mean_ms: mean,
        std_ms: var.sqrt(),
        per_tx: per_tx.iter().map(|(s, g)| PerTxRecord { success: *s, gas: *g }).collect(),
    }
}


pub const OPT: OptimizationLevel = OptimizationLevel::Aggressive;

pub fn opt_tag(opt: OptimizationLevel) -> &'static str {
    match opt {
        OptimizationLevel::None => "o0",
        OptimizationLevel::Less => "o1",
        OptimizationLevel::Default => "o2",
        OptimizationLevel::Aggressive => "o3",
    }
}

// ── Skeleton Registry ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
pub struct SkeletonRegistry {
    pub groups: Vec<RegistryGroup>,
}

#[derive(Serialize, Deserialize)]
pub struct RegistryGroup {
    pub skeleton_hash: u64,
    pub member_hashes: Vec<[u8; 32]>,
    /// Variance per PUSH: 0 = Invariant, N > 0 = Variant(table_index = N - 1).
    push_variance: Vec<u32>,
    pub num_variant: u32,
}

impl RegistryGroup {
    pub fn from_analysis(
        skeleton_hash: u64,
        member_hashes: &[B256],
        variance: &SkeletonVariance,
    ) -> Self {
        let push_variance = variance
            .pushes
            .iter()
            .map(|p| match p {
                PushClassification::Invariant => 0,
                PushClassification::Variant { table_index } => table_index + 1,
            })
            .collect();
        Self {
            skeleton_hash,
            member_hashes: member_hashes.iter().map(|h| h.0).collect(),
            push_variance,
            num_variant: variance.num_variant,
        }
    }
}

pub fn registry_path(cache_dir: &Path) -> PathBuf {
    cache_dir.join("skeleton_registry.bin")
}

pub fn save_registry(cache_dir: &Path, registry: &SkeletonRegistry) {
    std::fs::create_dir_all(cache_dir).ok();
    let data = bincode::serialize(registry).expect("serialize registry");
    std::fs::write(registry_path(cache_dir), data).expect("write registry");
}

// ── Skeleton AOT Cache ───────────────────────────────────────────────────────

pub struct SkeletonCacheArtifacts {
    pub symbol: String,
    pub object: PathBuf,
    pub library: PathBuf,
}

pub fn skeleton_cache_artifacts(cache_dir: &Path, skeleton_hash: u64) -> SkeletonCacheArtifacts {
    let tag = opt_tag(OPT);
    let spec_tag = format!("{:?}", crate::bin_common::ETH_SPEC).to_lowercase();
    let key = format!("skel_{skeleton_hash:016x}__{spec_tag}__{tag}");
    let stem = cache_dir.join(&key);
    SkeletonCacheArtifacts {
        symbol: format!("skel_{skeleton_hash:016x}"),
        object: stem.with_extension("o"),
        library: stem.with_extension(std::env::consts::DLL_EXTENSION),
    }
}

pub fn compile_skeleton_to_cache(
    bytecode: &[u8],
    variance: &SkeletonVariance,
    artifacts: &SkeletonCacheArtifacts,
) -> Result<(), String> {
    if let Some(parent) = artifacts.object.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend =
        EvmLlvmBackend::new(context, true, OPT).map_err(|e| format!("AOT backend: {e}"))?;
    let mut compiler = EvmCompiler::new(backend);
    compiler
        .translate_skeleton(&artifacts.symbol, bytecode, crate::bin_common::ETH_SPEC, variance)
        .map_err(|e| format!("translate_skeleton: {e}"))?;
    compiler.write_object_to_file(&artifacts.object).map_err(|e| format!("write: {e}"))?;
    revmc::Linker::new()
        .link(&artifacts.library, [&artifacts.object])
        .map_err(|e| format!("link: {e}"))?;
    Ok(())
}

pub fn load_skeleton_cached(
    artifacts: &SkeletonCacheArtifacts,
) -> Result<(RawEvmCompilerFn, libloading::Library), String> {
    let library = unsafe { libloading::Library::new(&artifacts.library) }
        .map_err(|e| format!("dlopen: {e}"))?;
    let symbol = unsafe { library.get::<RawEvmCompilerFn>(artifacts.symbol.as_bytes()) }
        .map_err(|e| format!("dlsym: {e}"))?;
    let function = *symbol;
    drop(symbol);
    Ok((function, library))
}

