//! Persistent per-hash AOT cache: compile-or-load contracts as cached `.so`
//! files. Split out of `bin_common.rs`, which re-exports everything here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use revm::bytecode::Bytecode;
use revm::primitives::B256;
use revmc::{EvmCompiler, EvmLlvmBackend, OptimizationLevel};
use revmc_context::RawEvmCompilerFn;

use super::{should_skip_compilation, CompiledContracts, ETH_SPEC};

struct CacheArtifacts {
    key: String,
    symbol: String,
    object: PathBuf,
    library: PathBuf,
}

fn cache_artifacts(cache_dir: &Path, hash: &B256, opt: OptimizationLevel) -> CacheArtifacts {
    let hash_hex = hex::encode(hash);
    let spec_tag = format!("{ETH_SPEC:?}").to_lowercase();
    let opt_tag = match opt {
        OptimizationLevel::None => "o0",
        OptimizationLevel::Less => "o1",
        OptimizationLevel::Default => "o2",
        OptimizationLevel::Aggressive => "o3",
    };
    let key = format!("{hash_hex}__{spec_tag}__{opt_tag}");
    let stem = cache_dir.join(&key);
    CacheArtifacts {
        key,
        symbol: format!("c_{hash_hex}"),
        object: stem.with_extension("o"),
        library: stem.with_extension(std::env::consts::DLL_EXTENSION),
    }
}

fn load_cached_symbol(
    artifacts: &CacheArtifacts,
) -> Result<(RawEvmCompilerFn, libloading::Library), String> {
    let library = unsafe { libloading::Library::new(&artifacts.library) }
        .map_err(|e| format!("dlopen {}: {e}", artifacts.library.display()))?;
    let symbol = unsafe { library.get::<RawEvmCompilerFn>(artifacts.symbol.as_bytes()) }
        .map_err(|e| format!("dlsym {}: {e}", artifacts.symbol))?;
    let function = *symbol;
    drop(symbol);
    Ok((function, library))
}

fn compile_contract_to_cache(
    bytecode: &Bytecode,
    opt: OptimizationLevel,
    artifacts: &CacheArtifacts,
) -> Result<(), String> {
    if let Some(parent) = artifacts.object.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend =
        EvmLlvmBackend::new(context, true, opt).map_err(|e| format!("AOT backend: {e}"))?;
    let mut compiler = EvmCompiler::new(backend);
    compiler
        .translate(&artifacts.symbol, bytecode.original_byte_slice(), ETH_SPEC)
        .map_err(|e| format!("translate {}: {e}", artifacts.key))?;
    compiler
        .write_object_to_file(&artifacts.object)
        .map_err(|e| format!("write {}: {e}", artifacts.object.display()))?;
    revmc::Linker::new()
        .link(&artifacts.library, [&artifacts.object])
        .map_err(|e| format!("link {}: {e}", artifacts.library.display()))?;
    Ok(())
}

fn compile_jit_fallback(
    hash: B256,
    bytecode: &Bytecode,
    opt: OptimizationLevel,
) -> Option<RawEvmCompilerFn> {
    let symbol = format!("c_{}", hex::encode(hash));
    let context = Box::leak(Box::new(revmc::llvm::inkwell::context::Context::create()));
    let backend = EvmLlvmBackend::new(context, false, opt).ok()?;
    let compiler: &'static mut EvmCompiler<EvmLlvmBackend<'static>> =
        Box::leak(Box::new(EvmCompiler::new(backend)));
    let func_id = compiler.translate(&symbol, bytecode.original_byte_slice(), ETH_SPEC).ok()?;
    unsafe { compiler.jit_function(func_id).ok().map(|f| f.into_inner()) }
}

pub fn compile_all_contracts_with_cache(
    code_values: &HashMap<B256, Bytecode>,
    opt: OptimizationLevel,
    cache_dir: &Path,
) -> CompiledContracts {
    std::fs::create_dir_all(cache_dir).ok();

    let mut contracts: Vec<(B256, &Bytecode)> = code_values
        .iter()
        .filter(|(_, bc)| !bc.is_empty() && !should_skip_compilation(bc))
        .map(|(h, bc)| (*h, bc))
        .collect();
    contracts.sort_by_key(|(_, bc)| std::cmp::Reverse(bc.original_byte_slice().len()));

    let mut cached = Vec::new();
    let mut to_compile: Vec<(B256, &Bytecode, CacheArtifacts)> = Vec::new();
    for (hash, bytecode) in &contracts {
        let artifacts = cache_artifacts(cache_dir, hash, opt);
        if artifacts.object.exists() && artifacts.library.exists() {
            cached.push((*hash, artifacts));
        } else {
            to_compile.push((*hash, *bytecode, artifacts));
        }
    }

    let mut functions = HashMap::new();
    let mut libraries = Vec::new();
    let mut hits = 0usize;
    for (hash, artifacts) in cached {
        match load_cached_symbol(&artifacts) {
            Ok((f, lib)) => {
                functions.insert(hash, f);
                libraries.push(lib);
                hits += 1;
            }
            Err(e) => {
                eprintln!("  Cache stale {}..: {e}", &hex::encode(hash)[..16]);
                let bytecode = code_values.get(&hash).unwrap();
                to_compile.push((hash, bytecode, artifacts));
            }
        }
    }

    let n_threads = std::thread::available_parallelism().map(|n| n.get().min(16)).unwrap_or(4);
    eprintln!("  Cache: hits={hits} to_compile={} threads={n_threads}", to_compile.len());

    if to_compile.is_empty() {
        return CompiledContracts { functions: Arc::new(functions), _libraries: libraries };
    }

    let mut assignments: Vec<Vec<(B256, &Bytecode, CacheArtifacts)>> =
        (0..n_threads).map(|_| Vec::new()).collect();
    for (i, item) in to_compile.into_iter().enumerate() {
        assignments[i % n_threads].push(item);
    }

    let thread_results: Vec<Vec<(B256, RawEvmCompilerFn, Option<libloading::Library>)>> =
        std::thread::scope(|s| {
            let handles: Vec<_> = assignments
                .into_iter()
                .enumerate()
                .map(|(tid, chunk)| {
                    s.spawn(move || {
                        let mut results = Vec::with_capacity(chunk.len());
                        for (hash, bytecode, artifacts) in chunk {
                            if let Err(e) = compile_contract_to_cache(bytecode, opt, &artifacts) {
                                eprintln!(
                                    "  [T{tid}] AOT fail {}..: {e}",
                                    &hex::encode(hash)[..16]
                                );
                                if let Some(f) = compile_jit_fallback(hash, bytecode, opt) {
                                    results.push((hash, f, None));
                                }
                                continue;
                            }
                            match load_cached_symbol(&artifacts) {
                                Ok((f, lib)) => results.push((hash, f, Some(lib))),
                                Err(e) => {
                                    eprintln!(
                                        "  [T{tid}] load fail {}..: {e}",
                                        &hex::encode(hash)[..16]
                                    );
                                    if let Some(f) = compile_jit_fallback(hash, bytecode, opt) {
                                        results.push((hash, f, None));
                                    }
                                }
                            }
                        }
                        results
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

    for thread_result in thread_results {
        for (hash, f, lib) in thread_result {
            functions.insert(hash, f);
            if let Some(lib) = lib {
                libraries.push(lib);
            }
        }
    }

    eprintln!("  Total: {} compiled (hits={hits})", functions.len());

    CompiledContracts { functions: Arc::new(functions), _libraries: libraries }
}

