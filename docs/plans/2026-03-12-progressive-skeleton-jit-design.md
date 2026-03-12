# Progressive Skeleton JIT with ORC v2

**Date**: 2026-03-12
**Status**: Draft
**Branch**: jit-integration
**Prerequisite**: Skeleton-Aware Compilation (Phase 1, completed 2026-03-09)

## Problem

Phase 1 skeleton compilation achieves 6.2x compile speedup and 1.16-1.28x execution speedup,
but requires **pre-scanning all contracts in a block** to group bytecodes and analyze variance
before any compilation begins. This batch model cannot support dynamic/online execution where
contracts arrive one at a time.

Additionally, the current JIT backend uses `inkwell::ExecutionEngine` (MCJIT), which is
deprecated in LLVM and lacks:
- Incremental module management (add/remove modules at any time)
- Per-compilation-unit resource tracking (selective code eviction)
- Thread-safe module submission (concurrent `add_module()`)

## Solution: Progressive Skeleton JIT

A three-tier compilation system with deferred variance analysis and ORC v2 runtime:

```
Tier 0: Interpreter         — zero latency, first execution of any contract
Tier 1: Per-Hash JIT (O2)   — full constant folding, background compile, per-contract
Tier 2: Skeleton JIT (O2)   — deferred variance analysis, one compilation reused by N contracts
```

**Key insight**: Tier 1 (per-hash) gives every contract optimal JIT code with full constant
folding. Tier 2 (skeleton) eliminates compilation entirely for new contracts whose skeleton
is already known — only a microsecond-level `build_data_table()` is needed.

The two tiers complement each other:
- Tier 1 serves known contracts with maximal optimization
- Tier 2 serves *unknown* contracts with zero compilation latency

## Architecture Overview

```
┌──────────────────────────────────────────────────────────┐
│                    SkeletonRegistry                      │
│                                                          │
│  per_hash: DashMap<B256, CachedFn>        (Tier 1)       │
│  skeletons: DashMap<u64, SkeletonEntry>   (Tier 2)       │
│  compile_tx: Sender<CompileRequest>       (async queue)  │
│                                                          │
└───────────────────────┬──────────────────────────────────┘
                        │
        ┌───────────────┼───────────────┐
        ▼               ▼               ▼
   ┌─────────┐   ┌───────────┐   ┌───────────────┐
   │ Resolve  │   │ Background│   │  ORC v2 LLJIT │
   │ Hot Path │   │ Compiler  │   │  (Backend)    │
   │          │   │ Thread    │   │               │
   │ per-hash │   │ Pool      │   │ add_module()  │
   │ → skel   │   │           │   │ lookup()      │
   │ → interp │   │ Tier 1/2  │   │ RT.remove()   │
   └─────────┘   │ compile   │   └───────────────┘
                  └───────────┘
```

## Module 1: ORC v2 Backend

**Goal**: Replace `ExecutionEngine` (MCJIT) with `LLJIT` (ORC v2) in `EvmLlvmBackend`.

### Current State

```rust
// crates/revmc-llvm/src/lib.rs:125-138
let exec_engine = module.create_jit_execution_engine(opt_level);  // MCJIT

// jit_function: gets address from single module
let addr = self.exec_engine().get_function_address(name)?;

// free_all_functions: destroys entire module + engine
```

Problems:
- One module per backend, `finalized` flag prevents adding more functions
- `free_fn_machine_code()` is coarse-grained; `free_all_functions()` destroys everything
- `Box::leak()` hack in bin_common.rs to keep backend alive for function pointers

### New Design

```rust
// crates/revmc-llvm/src/orc_backend.rs (new file)

pub struct EvmOrcBackend<'ctx> {
    // ORC v2 engine
    lljit: OrcLLJIT,

    // Compilation support (reused across modules)
    machine: TargetMachine,
    opt_level: OptimizationLevel,

    // Track compiled modules for cleanup
    trackers: HashMap<u32, ResourceTracker>,  // func_id → RT
    functions: HashMap<u32, String>,           // func_id → symbol name
    function_counter: u32,

    // Types (shared, same as current backend)
    ty_void: VoidType<'ctx>,
    ty_i1: IntType<'ctx>,
    // ... etc
}
```

Key changes to the `Backend` trait implementation:

```rust
impl Backend for EvmOrcBackend<'_> {
    fn build_function(&mut self, name: &str, ...) -> Result<(Builder, FuncId)> {
        // Create a fresh module + context for this function
        // (each function gets its own module for independent compilation)
        let ts_ctx = ThreadSafeContext::new();
        let module = ts_ctx.create_module(name);
        // ... build function in module, return builder + id
    }

    fn optimize_module(&mut self) -> Result<()> {
        // Run passes on the current pending module
        module.run_passes(pass_string, &self.machine, opts)
    }

    fn jit_function(&mut self, id: FuncId) -> Result<usize> {
        // Wrap module as ThreadSafeModule, add to LLJIT with ResourceTracker
        let tsm = ThreadSafeModule::new(module, ts_ctx);
        let rt = self.lljit.get_main_jit_dylib().create_resource_tracker();
        self.lljit.add_module_with_rt(tsm, rt.clone())?;
        self.trackers.insert(id, rt);

        // Lookup symbol address
        let addr = self.lljit.lookup(name)?;
        Ok(addr as usize)
    }

    fn free_function(&mut self, id: FuncId) -> Result<()> {
        // Precise removal via ResourceTracker
        if let Some(rt) = self.trackers.remove(&id) {
            rt.remove()?;
        }
        Ok(())
    }

    fn free_all_functions(&mut self) -> Result<()> {
        // Clear all via JITDylib
        self.lljit.get_main_jit_dylib().clear()?;
        self.trackers.clear();
        Ok(())
    }
}
```

### Concurrency Model

ORC v2's `add_module()` is thread-safe at the ExecutionSession level. For parallel
compilation with Rayon:

```rust
// Each Rayon worker:
//   1. Creates independent ThreadSafeContext (no shared lock)
//   2. Builds module + runs LLVM O2 (CPU-bound, parallel)
//   3. Calls lljit.add_module() (thread-safe, briefly locked)
//   4. Calls lljit.lookup() (thread-safe)
```

No `Box::leak()` hack needed — LLJIT owns the compiled code, function pointers remain
valid until `ResourceTracker::remove()`.

### Migration Path

1. `EvmOrcBackend` implements the same `Backend` trait as `EvmLlvmBackend`
2. `EvmCompiler<EvmOrcBackend>` works with zero changes to compiler frontend
3. Old `EvmLlvmBackend` kept temporarily for A/B testing, removed after validation

### ORC v2 Bindings Status

All required bindings exist in `crates/revmc-llvm/src/orc.rs` (1448 lines):

| Feature | Bound? | Used by this design? |
|---------|--------|---------------------|
| ThreadSafeContext | Yes | Yes — per-module isolation |
| ThreadSafeModule | Yes | Yes — wraps compiled modules |
| LLJIT (add/lookup) | Yes | Yes — core JIT engine |
| ResourceTracker | Yes | Yes — per-function eviction |
| IRTransformLayer | Yes | No — optimization done before add_module |
| LazyCallThroughManager | No | No — interpreter fallback is sufficient |
| IndirectStubsManager | No | No — not needed |

## Module 2: SkeletonRegistry

**Goal**: Runtime skeleton management with deferred variance analysis.

### State Machine

```
                   ┌──────────────────┐
   new skeleton ──→│    Collecting     │
                   │ samples: Vec<..> │
                   │ count < N (=2)   │
                   └────────┬─────────┘
                            │ count == N
                            ▼
                   ┌──────────────────┐
                   │    Compiling     │
                   │ (transient)      │
                   │ analyze_group()  │
                   │ translate_skel() │
                   └────────┬─────────┘
                            │ compile done
                            ▼
                   ┌──────────────────┐
                   │    Compiled v0   │◄──── invariant violation
                   │ fn_ptr           │         detected
                   │ variance         │            │
                   │ invariant_values │            ▼
                   │ version: 0       │   ┌───────────────┐
                   └──────────────────┘   │ Recompiling   │
                            ▲             │ new samples   │
                            │             │ re-analyze()  │
                            │             └───────┬───────┘
                            │                     │
                            └─────────────────────┘
                              Compiled v1 (replaces v0)
```

### Data Structures

```rust
use dashmap::DashMap;
use std::sync::atomic::AtomicU64;

/// Central registry for progressive skeleton JIT.
pub struct SkeletonRegistry {
    /// Tier 1: per-hash compiled functions.
    per_hash: DashMap<B256, CachedFn>,

    /// Tier 2: skeleton state machine.
    skeletons: DashMap<u64, SkeletonEntry>,

    /// Async compilation queue.
    compile_tx: crossbeam::channel::Sender<CompileRequest>,
}

pub struct CachedFn {
    pub fn_ptr: RawEvmCompilerFn,
    pub resource_tracker: ResourceTracker,
}

pub enum SkeletonEntry {
    /// Collecting samples, not yet compiled.
    Collecting {
        /// Bytecode samples: (code_hash, bytecode).
        /// Kept for variance analysis and potential recompilation.
        samples: Vec<(B256, Vec<u8>)>,
    },
    /// Skeleton compiled and available.
    Compiled(CompiledSkeleton),
}

pub struct CompiledSkeleton {
    /// Compiled function pointer (shared by all instances).
    pub fn_ptr: RawEvmCompilerFn,

    /// Precise variance classification from sample analysis.
    pub variance: SkeletonVariance,

    /// Expected values for each Invariant position.
    /// Used by validate_invariants() to detect violations.
    pub invariant_values: Vec<U256>,

    /// All known samples (kept for recompilation on violation).
    pub samples: Vec<(B256, Vec<u8>)>,

    /// ORC v2 resource tracker for this skeleton's compiled code.
    pub resource_tracker: ResourceTracker,

    /// Recompilation version counter.
    pub version: u32,

    /// Outlier protection: invariant violation counter.
    pub violation_count: AtomicU32,

    /// Total resolve attempts via this skeleton (for violation rate calculation).
    pub resolve_count: AtomicU64,
}

pub enum CompileRequest {
    /// Compile a single contract (Tier 1).
    PerHash { hash: B256, bytecode: Vec<u8> },
    /// Compile a skeleton from collected samples (Tier 2).
    Skeleton { skel_hash: u64, samples: Vec<(B256, Vec<u8>)> },
    /// Recompile skeleton with updated samples after invariant violation.
    Recompile { skel_hash: u64, new_sample: (B256, Vec<u8>) },
}
```

### Sample Collection

When a new contract arrives and its skeleton has < N samples:

```rust
fn collect_sample(&self, skel_hash: u64, hash: B256, bytecode: &[u8]) {
    let mut entry = self.skeletons.entry(skel_hash).or_insert_with(|| {
        SkeletonEntry::Collecting { samples: Vec::new() }
    });

    if let SkeletonEntry::Collecting { samples } = entry.value_mut() {
        // Deduplicate by code_hash
        if samples.iter().any(|(h, _)| *h == hash) { return; }
        samples.push((hash, bytecode.to_vec()));

        if samples.len() >= SAMPLE_THRESHOLD {
            // Threshold reached — send to background compiler
            let samples_clone = samples.clone();
            self.compile_tx.send(CompileRequest::Skeleton {
                skel_hash,
                samples: samples_clone,
            }).ok();
        }
    }
}
```

### Invariant Validation

Microsecond-level check before using a skeleton function:

```rust
/// Verify that all Invariant PUSH positions in `bytecode` match expected values.
/// Returns false if any mismatch (the skeleton cannot be used for this bytecode).
///
/// Time complexity: O(bytecode_len), zero allocation.
pub fn validate_invariants(
    bytecode: &[u8],
    variance: &SkeletonVariance,
    expected: &[U256],
) -> bool {
    let mut push_idx = 0;
    let mut inv_idx = 0;
    let mut i = 0;
    while i < bytecode.len() {
        let op = bytecode[i];
        i += 1;
        if op >= 0x60 && op <= 0x7f {
            let n = (op - 0x5f) as usize;
            let end = (i + n).min(bytecode.len());
            if variance.pushes[push_idx] == PushClassification::Invariant {
                let val = U256::from_be_slice(&bytecode[i..end]);
                if val != expected[inv_idx] {
                    return false;
                }
                inv_idx += 1;
            }
            push_idx += 1;
            i = end;
        }
    }
    true
}
```

Cost: single linear scan of bytecode, no allocation. Typical: <1us for a 15KB contract.

### Invariant Violation Handling

When `validate_invariants()` returns false:

1. **Immediate**: the contract runs via interpreter this time (Tier 0 fallback)
2. **Background**: schedule per-hash compile for this specific contract (Tier 1)
3. **Recompile decision**: only recompile the skeleton if violation rate exceeds threshold

The violating contract always gets its own Tier 1 per-hash compilation (full constant
folding, optimal code). The skeleton is only recompiled when the variance analysis was
genuinely wrong — not for rare outliers.

#### Outlier Protection ("耗子屎" Defense)

A single outlier contract must not degrade the skeleton for thousands of normal instances.
Example: 3,323 UniV3 Pool contracts have PUSH[7]=0x42, but 1 outlier has PUSH[7]=0x99.
Without protection, recompiling would make PUSH[7] Variant for all 3,323 contracts —
destroying constant folding for a position that is invariant in 99.97% of cases.

```rust
struct CompiledSkeleton {
    // ... existing fields ...
    violation_count: AtomicU32,   // invariant violation counter
    resolve_count: AtomicU64,     // total resolve attempts via this skeleton
}
```

Recompile trigger logic:

```rust
if !validate_invariants(bytecode, &skel.variance, &skel.invariant_values) {
    let violations = skel.violation_count.fetch_add(1, Relaxed);
    let total = skel.resolve_count.load(Relaxed);

    // Only recompile if violation rate > 10% AND at least 10 violations observed.
    // This prevents a single outlier from degrading the skeleton.
    if violations > 10 && (violations as f64 / total as f64) > 0.10 {
        self.schedule_recompile(skel_hash, bytecode);
    }

    // The outlier always gets its own optimal per-hash compilation.
    self.schedule_per_hash(hash, bytecode);
    return Resolution::Interpreter;
}
```

| Scenario | Behavior |
|----------|----------|
| 1 outlier / 3323 normal (0.03%) | Outlier → per-hash; skeleton unchanged; 3322 keep full optimization |
| 500 variants / 3323 total (15%) | Violation rate > 10% → recompile; position is genuinely variant |
| 3rd sample violates after N=2 analysis | Violation rate = 33% → recompile; 2-sample analysis was inaccurate |

#### Recompile Flow (when triggered)

Uses copy-on-write to avoid holding DashMap locks during LLVM compilation:

```rust
// 1. Short read lock: clone samples
let (samples, old_version) = {
    let entry = registry.skeletons.get(&skel_hash).unwrap();
    // ... clone data, release lock immediately
};

// 2. Lock-free: LLVM compilation (seconds)
let mut all_samples = samples;
all_samples.push(new_sample);
let new_variance = analyze_skeleton_group(&all_samples);
let new_fn_ptr = compile_skeleton(..., &new_variance);

// 3. Short write lock: atomic swap
registry.skeletons.insert(skel_hash, SkeletonEntry::Compiled(CompiledSkeleton {
    fn_ptr: new_fn_ptr,
    variance: new_variance,
    samples: all_samples,
    version: old_version + 1,
    ...
}));
// Old ResourceTracker::remove() frees old machine code
```

The old skeleton version remains valid and in use for existing compatible contracts until
the new version is ready. No service disruption.

## Module 3: Resolve Hot Path

**Goal**: The fastest possible lookup from EVM execution to compiled function.

### Integration Point

The resolve function replaces the current `HashMap<B256, RawEvmCompilerFn>` lookup in
`JitHandler::run_exec_loop()` (bin_common.rs:369-431).

```rust
/// Resolution result for a contract execution request.
pub enum Resolution {
    /// Tier 1: direct call to per-hash compiled function.
    Direct(RawEvmCompilerFn),
    /// Tier 2: call skeleton function with per-instance data table.
    Skeleton(RawEvmCompilerFn, ImmDataTable),
    /// Tier 0: no compiled version available, use interpreter.
    Interpreter,
}

impl SkeletonRegistry {
    pub fn resolve(&self, hash: B256, bytecode: &[u8]) -> Resolution {
        // Fast path 1: per-hash cache (Tier 1) — DashMap lookup ~15ns
        if let Some(f) = self.per_hash.get(&hash) {
            return Resolution::Direct(f.fn_ptr);
        }

        // Fast path 2: skeleton compiled (Tier 2)
        let skel_hash = skeleton_hash(bytecode);
        if let Some(entry) = self.skeletons.get(&skel_hash) {
            if let SkeletonEntry::Compiled(skel) = entry.value() {
                if validate_invariants(bytecode, &skel.variance, &skel.invariant_values) {
                    let table = build_data_table(bytecode, &skel.variance);
                    return Resolution::Skeleton(skel.fn_ptr, table);
                } else {
                    // Invariant violated — fallback + schedule recompile
                    self.schedule_recompile(skel_hash, hash, bytecode);
                }
            }
            // Entry exists but still Collecting — fall through
        }

        // Slow path: collect sample, schedule per-hash compile
        self.collect_sample(skel_hash, hash, bytecode);
        self.schedule_per_hash(hash, bytecode);
        Resolution::Interpreter
    }
}
```

### Hot Path Performance

| Path | Latency | When |
|------|---------|------|
| Per-hash hit (Tier 1) | ~15ns (DashMap get) | Contract seen before, already compiled |
| Skeleton hit (Tier 2) | ~1-5us (validate + build_data_table) | New contract, known skeleton |
| Miss (Tier 0) | ~0 (just queues compile) | Completely new contract |

### Handler Integration

```rust
pub struct ProgressiveJitHandler {
    registry: Arc<SkeletonRegistry>,
}

impl Handler for ProgressiveJitHandler {
    fn run_exec_loop(&self, evm: &mut EvmInner) -> Result<CallOrResult> {
        loop {
            let bytecode = get_current_bytecode(evm);
            let hash = bytecode_hash(evm);

            match self.registry.resolve(hash, bytecode) {
                Resolution::Direct(fn_ptr) => {
                    let f = EvmCompilerFn::new(fn_ptr);
                    let action = unsafe { f.call_with_interpreter(...) };
                    handle_action(action)?;
                }
                Resolution::Skeleton(fn_ptr, table) => {
                    let f = EvmCompilerFn::new(fn_ptr);
                    // Set data table pointer for variant PUSH loads
                    ecx.imm_data_ptr = table.data.as_ptr();
                    let action = unsafe { f.call_with_interpreter_data(...) };
                    handle_action(action)?;
                }
                Resolution::Interpreter => {
                    let action = evm.frame_run()?;
                    handle_action(action)?;
                }
            }
        }
    }
}
```

## Module 4: Background Compiler

**Goal**: Async compilation thread pool that processes Tier 1 and Tier 2 requests.

### Architecture

```
                  compile_tx ──────► compile_rx
                                         │
                                    ┌────┴────┐
                                    │  Rayon   │
                                    │  Thread  │
                                    │  Pool    │
                                    ├─────────┤
                                    │ Worker 1 │  Each worker:
                                    │ Worker 2 │  - Own ThreadSafeContext
                                    │ Worker 3 │  - translate + O2 + add_module
                                    │ Worker 4 │  - Update registry atomically
                                    └─────────┘
```

### Compilation Flow

```rust
fn background_compiler(
    rx: Receiver<CompileRequest>,
    registry: Arc<SkeletonRegistry>,
    lljit: Arc<OrcLLJIT>,  // thread-safe
) {
    // Use rayon for parallel processing of compilation requests
    while let Ok(request) = rx.recv() {
        rayon::spawn(move || {
            match request {
                CompileRequest::PerHash { hash, bytecode } => {
                    // Tier 1: standard per-hash compilation
                    let ts_ctx = ThreadSafeContext::new();
                    let mut compiler = EvmCompiler::new(EvmOrcBackend::new(lljit, ts_ctx));
                    let id = compiler.translate(&format!("evm_{hash}"), &bytecode, spec).unwrap();
                    let fn_ptr = unsafe { compiler.jit_function(id) }.unwrap();
                    registry.per_hash.insert(hash, CachedFn { fn_ptr, ... });
                }

                CompileRequest::Skeleton { skel_hash, samples } => {
                    // Tier 2: skeleton compilation
                    let bytecodes: Vec<&[u8]> = samples.iter().map(|(_, b)| b.as_slice()).collect();
                    let variance = analyze_skeleton_group(&bytecodes);

                    let ts_ctx = ThreadSafeContext::new();
                    let mut compiler = EvmCompiler::new(EvmOrcBackend::new(lljit, ts_ctx));
                    let id = compiler.translate_skeleton(
                        &format!("skel_{skel_hash:016x}"),
                        &bytecodes[0], spec, &variance,
                    ).unwrap();
                    let fn_ptr = unsafe { compiler.jit_function(id) }.unwrap();

                    let invariant_values = extract_invariant_values(&bytecodes[0], &variance);
                    registry.skeletons.insert(skel_hash, SkeletonEntry::Compiled(CompiledSkeleton {
                        fn_ptr, variance, invariant_values, samples, version: 0, ..
                    }));
                }

                CompileRequest::Recompile { skel_hash, new_sample } => {
                    // Copy-on-write: short read lock to clone data, then compile lock-free.
                    let (mut samples, old_version) = {
                        let entry = registry.skeletons.get(&skel_hash).unwrap();
                        if let SkeletonEntry::Compiled(skel) = entry.value() {
                            (skel.samples.clone(), skel.version)
                        } else { continue; }
                    }; // read lock released here

                    // Lock-free: re-analyze + LLVM compilation (seconds)
                    samples.push(new_sample);
                    let bytecodes: Vec<&[u8]> = samples.iter()
                        .map(|(_, b)| b.as_slice()).collect();
                    let new_variance = analyze_skeleton_group(&bytecodes);

                    let ts_ctx = ThreadSafeContext::new();
                    let mut compiler = EvmCompiler::new(EvmOrcBackend::new(lljit, ts_ctx));
                    let id = compiler.translate_skeleton(
                        &format!("skel_{skel_hash:016x}_v{}", old_version + 1),
                        &bytecodes[0], spec, &new_variance,
                    ).unwrap();
                    let new_fn_ptr = unsafe { compiler.jit_function(id) }.unwrap();

                    // Short write lock: atomic swap
                    let invariant_values = extract_invariant_values(&bytecodes[0], &new_variance);
                    let old_entry = registry.skeletons.insert(skel_hash,
                        SkeletonEntry::Compiled(CompiledSkeleton {
                            fn_ptr: new_fn_ptr,
                            variance: new_variance,
                            invariant_values,
                            samples,
                            resource_tracker: new_rt,
                            version: old_version + 1,
                            violation_count: AtomicU32::new(0),
                            resolve_count: AtomicU64::new(0),
                        })
                    );
                    // Free old machine code
                    if let Some(SkeletonEntry::Compiled(old)) = old_entry {
                        old.resource_tracker.remove().ok();
                    }
                }
            }
        });
    }
}
```

## Lifecycle Example

```
Block N: 500 contracts to execute
═══════════════════════════════════

t=0   Contract A (first time, skeleton S₁)
      → resolve: miss
      → Tier 0: interpreter
      → Background: schedule per-hash(A), collect_sample(S₁, A)

t=1   Contract B (first time, same skeleton S₁)
      → resolve: per-hash miss, skeleton S₁ still Collecting
      → Tier 0: interpreter
      → Background: schedule per-hash(B), collect_sample(S₁, B)
      → samples.len() == 2 == THRESHOLD → schedule skeleton(S₁)

t=2   Contract A (second call)
      → resolve: per-hash(A) compiled! → Tier 1: direct call
      → Full constant folding, optimal code

t=3   Contract C (first time, same skeleton S₁)
      → resolve: per-hash miss, skeleton S₁ now Compiled
      → validate_invariants(C, S₁) → ✅
      → Tier 2: build_data_table(C) + call skeleton fn
      → Zero LLVM compilation for C!

t=99  Contract X (skeleton S₁, but PUSH[7] differs from invariant)
      → resolve: validate_invariants(X, S₁) → ❌
      → Tier 0: interpreter
      → Background: schedule per-hash(X), schedule recompile(S₁ + X)

t=100 Skeleton S₁ v1 ready (PUSH[7] now Variant)
      → Future contracts with S₁: use v1 + data table
```

## Performance Expectations

### Compilation Latency

| Scenario | Current (batch AOT) | Progressive Skeleton JIT |
|----------|--------------------|-----------------------|
| First call to new contract | Blocked until all compiled | Immediate (interpreter) |
| Second call to known contract | Already compiled | Tier 1 ready (~ms background) |
| New contract, known skeleton | Blocked until all compiled | Tier 2: ~1-5us (data table only) |
| Invariant violation | N/A | One interpreter call + background recompile |

### Steady-State Memory

| Component | Size |
|-----------|------|
| Per-hash fn (Tier 1) | ~5-50KB machine code per contract |
| Skeleton fn (Tier 2) | ~5-50KB shared across N contracts |
| ImmDataTable | ~32 bytes × num_variant_pushes per contract (~1KB typical) |
| SkeletonEntry metadata | ~200 bytes per skeleton |
| Samples (kept for recompile) | ~15KB × N per skeleton (bytecode copies) |

### Cold Code Eviction (via ResourceTracker)

For long-running processes, contracts not called in the last M blocks can be evicted:

```rust
fn evict_cold_contracts(&self, block_threshold: u64, current_block: u64) {
    // Evict per-hash entries
    self.per_hash.retain(|_, cached| {
        if current_block - cached.last_used > block_threshold {
            cached.resource_tracker.remove().ok();
            false
        } else {
            true
        }
    });

    // Evict cold skeletons
    self.skeletons.retain(|_, entry| {
        if let SkeletonEntry::Compiled(skel) = entry {
            if current_block - skel.last_used > block_threshold {
                skel.resource_tracker.remove().ok();
                return false;
            }
        }
        true
    });
}
```

## Key Invariants

1. **Correctness**: Tier 2 skeleton code produces identical results to Tier 1 per-hash code
   for any contract that passes `validate_invariants()`. This is guaranteed by the existing
   skeleton compilation design (Phase 1, verified with 228/228 tx match).

2. **Monotonic progress**: A contract never regresses — once compiled (Tier 1 or 2),
   it stays compiled until explicitly evicted.

3. **No stale pointers**: ORC v2 `ResourceTracker::remove()` invalidates function pointers.
   Atomic swap in registry ensures no concurrent caller uses a freed pointer.

4. **SKIP_LOGIC safety**: Static jump target PUSHes are always skeleton-invariant
   (same jump targets for same opcode structure). `apply_variance()` asserts this.

## Files Changed

| File | Change | Module |
|------|--------|--------|
| `crates/revmc-llvm/src/orc_backend.rs` | NEW: EvmOrcBackend implementing Backend trait | 1 |
| `crates/revmc-llvm/src/lib.rs` | Add orc_backend module, keep old backend | 1 |
| `crates/revmc/src/registry.rs` | NEW: SkeletonRegistry, SkeletonEntry, resolve() | 2, 3 |
| `crates/revmc/src/skeleton.rs` | Add validate_invariants(), extract_invariant_values() | 2 |
| `crates/revmc/src/lib.rs` | Re-export registry module | 2 |
| `crates/revmc-context/src/lib.rs` | No changes (imm_data_ptr already exists) | — |
| `examples/runner/src/bin_common.rs` | ProgressiveJitHandler using SkeletonRegistry | 3 |
| `examples/runner/src/bin/progressive_bench.rs` | NEW: benchmark progressive vs batch | 4 |

## Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| ORC v2 Backend API mismatch with existing Backend trait | Implement same trait; old backend kept for A/B |
| DashMap contention on hot resolve path | Per-hash path is read-only; skeleton path is read-mostly |
| Invariant recompile storm (many violations at once) | Outlier protection: recompile only when violation rate > 10% AND > 10 violations |
| Single outlier degrades skeleton for all instances | Outlier uses per-hash (full optimization); skeleton untouched unless violation rate exceeds threshold |
| DashMap lock held during LLVM compilation | Copy-on-write: clone data under short read lock, compile lock-free, atomic swap under short write lock |
| Sample bytecode memory (kept for recompile) | Cap at 10 samples per skeleton; evict oldest |
| ResourceTracker removal while function executing | Atomic swap in registry; old RT removed after grace period |
| LLJIT symbol name collision across skeletons | Unique names: `skel_{hash}_v{version}`, `evm_{code_hash}` |

## Verification Strategy

1. **Unit tests**: validate_invariants() with known invariant/variant patterns
2. **Correctness test**: Run same block through (a) batch skeleton and (b) progressive skeleton,
   compare all tx results (gas, return data, state root)
3. **Latency benchmark**: Measure first-call latency (interpreter), second-call latency (Tier 1),
   and new-contract-known-skeleton latency (Tier 2 data_table only)
4. **Stress test**: Feed 10K contracts through progressive pipeline, verify no invariant violations
   go undetected, verify recompile produces correct results
5. **Memory test**: Run for 1000 blocks with eviction, verify no use-after-free via ASAN

## Non-Goals (Explicit)

- **Tiered optimization (O0 → O2)**: PGO experiments proved marginal benefit; single-level O2
- **Lazy compilation stubs**: Interpreter fallback achieves the same UX without complexity
- **ORC built-in thread pool**: C API lacks `setNumCompileThreads`; Rayon is equivalent
- **Sub-function compilation**: One contract = one LLVM function; splitting adds complexity
  for inter-function stack/gas passing with unclear benefit
- **Persistent AOT cache integration**: Phase 3 concern; this design focuses on in-process JIT
