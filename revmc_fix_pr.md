# revmc Bug Fix PR Analysis Report

## Overview

This PR contains 5 bug fixes for revmc (Rust EVM Compiler), covering correctness, safety, and compatibility issues in the JIT/AOT compilation execution paths.

---

## Bug 1: `call_with_interpreter` Fails to Persist `resume_at`, Causing Re-execution from Start After CALL/CREATE

**Commit:** `8b6b1bba`

### Root Cause

In `crates/revmc-context/src/lib.rs`, the `EvmCompilerFn::call_with_interpreter` function invokes JIT-compiled native code to execute EVM bytecode. When the JIT function encounters a suspending operation like `CALL`/`CREATE`, it updates `ecx.resume_at` to record the current execution position. However, the original code **never wrote `ecx.resume_at` back to the interpreter's bytecode PC** after the JIT function returned. This caused the interpreter to restart from position 0 on the next re-entry instead of resuming from the suspension point.

### Trigger Scenario

Take a Uniswap V2 Router contract executing `swapExactTokensForTokens` as an example:

1. The Router contract hits a `CALL` instruction (calling the Pair contract's `swap`)
2. The JIT function suspends and sets `resume_at` to the PC position right after CALL
3. The Pair contract's `swap` completes, control returns to the Router
4. **BUG:** The Router contract restarts execution from PC=0 instead of resuming after CALL
5. The contract executes repeatedly from the beginning, resulting in extreme slowdown (~921µs vs ~29µs after fix) or incorrect results

### Fix

After the JIT function returns, persist `ecx.resume_at` back to the interpreter's PC via `interpreter.bytecode.absolute_jump(resume_at)`:

```rust
// Save resume_at (Copy usize) before ecx's borrow ends.
let resume_at = ecx.resume_at;
// ...
// Persist the resume_at value in the interpreter's bytecode PC
interpreter.bytecode.absolute_jump(resume_at);
```

**Files changed:** `crates/revmc-context/src/lib.rs`, `examples/runner/src/bench_json_test.rs`

---

## Bug 2: Gas Mismatch in JIT CALL Builtin vs Interpreter

**Commit:** `59a6ee3d`

### Root Cause

In `crates/revmc-builtins/src/lib.rs`, the `__revmc_builtin_call` function handles `CALL`/`DELEGATECALL` and other call-family instructions in the JIT path. The original implementation used a single `gas::call_cost()` helper to compute gas cost in one shot, but this calculation diverged from the actual interpreter execution path in three ways:

1. **Missing separate base access cost deduction:** The interpreter charges static opcode gas (e.g., warm storage read cost = 100 post-Berlin) before entering call helpers. Since revmc marks CALL as dynamic gas, the builtin must charge this fee itself — but didn't.
2. **Missing `load_account_delegated` for EIP-7702 delegation:** The interpreter uses this function to load the target account and resolve the actual bytecode/code_hash (handling EIP-7702 delegation) while computing dynamic gas (cold/warm account access). The original implementation skipped this entirely.
3. **Hardcoded `call_stipend`:** Used a hardcoded `gas::CALL_STIPEND` constant instead of querying `gas_params()`.

### Trigger Scenario

When a transaction calls an EIP-7702 delegated contract post-Berlin:

1. Contract A executes `CALL` targeting address B, which has EIP-7702 delegation set to contract C
2. Interpreter path: charges 100 gas (warm read) first, then resolves to C's bytecode via `load_account_delegated` and computes cold/warm access gas
3. JIT path (before fix): uses `gas::call_cost()` as a single computation, missing the base access cost and skipping delegation resolution
4. **Result:** JIT and interpreter paths produce different `gas_used` values; in edge cases, one path may OOG while the other doesn't

### Fix

Rewrote the gas calculation logic to precisely match the interpreter's execution path:
- Deduct `base_access_cost` separately (selecting warm_storage_read_cost / 700 / 40 based on spec version)
- Deduct `transfer_value_cost` separately
- Call `load_account_delegated` to get dynamic gas and resolve actual bytecode
- Pass `(code_hash, bytecode)` into `CallInputs::known_bytecode` to avoid redundant loading

**Files changed:** `crates/revmc-builtins/src/lib.rs`, `crates/revmc-llvm/src/lib.rs`, `examples/runner/src/bin/gas_debug.rs`

---

## Bug 3: LLVM i256 Memory Operations and Overly Aggressive Alias Attributes Cause Silent Correctness Errors

**Commit:** `c4986258`

### Root Cause

This bug consists of two interrelated sub-issues:

**Sub-issue A — i256 load/store incorrectly transformed by LLVM optimizer:**

EVM stack words (EvmWord) are 32 bytes (i256) with 8-byte alignment. The original implementation used direct LLVM `load i256` / `store i256` instructions with `align 8` and `volatile`. However, LLVM at certain optimization levels could still split or vectorize i256 operations into incorrect memory access patterns (e.g., using AVX 256-bit instructions that assume 32-byte alignment).

**Sub-issue B — `noalias` and `speculatable` attributes too aggressive:**

The original implementation annotated JIT function parameters (`ecx`, `stack`, `stack_len`) with `noalias` and builtin functions with `Speculatable`. However:

- The `Gas` struct is accessible both through `ecx` (parameter 0) and directly as a parameter (parameter 3) — these are actually aliased. Similarly, `InputsImpl` is reachable through `EvmContext`. The `noalias` annotation caused LLVM to incorrectly assume they don't affect each other, enabling illegal load/store reordering or elimination.
- `Speculatable` means "no side effects, no undefined behavior, safe to speculatively execute." But builtin functions modify the gas counter, memory, etc., completely violating this contract.

### Trigger Scenario

```
PUSH32 <large_value>  // push a 256-bit value
DUP1                   // duplicate it
ADD                    // add them
```

Under `-O3` optimization, LLVM may:

1. Due to `noalias`, incorrectly assume `ecx` and another parameter point to different memory → reorder a store through `ecx` with a load through a direct parameter → read stale data
2. Due to `speculatable`, hoist a gas-charging builtin call before a conditional branch → gas is overcharged or charged on a path that should never execute

This manifests as certain contracts producing different computation results in JIT mode vs interpreter, especially at higher optimization levels.

### Fix

- **Split i256 into 4x i64 lane operations:** Instead of directly loading/storing i256, split into 4 individual i64 loads/stores, then combine with shift + or into i256. This ensures every memory operation is an 8-byte aligned i64 operation that LLVM won't attempt to vectorize or illegally transform.
- **Tighten `noalias`:** Only annotate `stack` (parameter 1) and `stack_len` (parameter 2) with `noalias`, as they genuinely don't alias other parameters. `ecx` (parameter 0) and `gas` (parameter 3) no longer carry the annotation.
- **Remove `Speculatable`:** Removed from both function-level and builtin-level attributes.

**Files changed:** `crates/revmc-builtins/src/ir.rs`, `crates/revmc-llvm/src/lib.rs`, `crates/revmc/src/compiler/mod.rs`

---

## Bug 4: CODECOPY Segfaults in AOT Mode Due to Compile-time Embedded Pointers

**Commit:** `6eb3a0e8`

### Root Cause

In `crates/revmc/src/compiler/translate.rs`, the `CODECOPY` instruction compilation embedded the contract bytecode's pointer and length **as constants directly into the generated machine code**:

```rust
let bytecode_ptr_int = self.bcx.uconst(self.isize_type, self.bytecode.code.as_ptr() as u64);
let bytecode_ptr = self.bcx.inttoptr(bytecode_ptr_int, self.ptr_type);
let bytecode_len = self.bcx.uconst(self.isize_type, self.bytecode.code.len() as u64);
```

This works fine in JIT mode (compilation and execution happen in the same process, pointer is valid). But in AOT mode, the compiled machine code is serialized to disk (`.o` → `.so` file). On subsequent loads, the original memory has long been freed, and the embedded pointer points to an invalid address → **segfault**.

### Trigger Scenario

1. First run: compile contract `0xABC...`, its bytecode is at heap address `0x7f1234000000`. CODECOPY inlines this pointer and writes to `.so` cache
2. Second run: load `.so` from cache, execute the contract
3. Contract hits `CODECOPY` instruction, attempts to read bytecode from `0x7f1234000000` → address is no longer valid → **SIGSEGV**

Any contract using `CODECOPY` will trigger this bug in AOT cache mode. For example, Solidity constructors typically CODECOPY the entire runtime bytecode into memory before RETURN.

### Fix

- Add `bytecode_ptr` and `bytecode_len` fields to the `EvmContext` struct (no longer embed pointers at compile time; read from context at runtime instead)
- Simplify `__revmc_builtin_codecopy` parameters from `(ecx, sp, bytecode_ptr, bytecode_len)` to `(ecx, sp)`, reading directly from `ecx.bytecode_ptr` / `ecx.bytecode_len` internally
- Dynamically populate the current bytecode's pointer and length each time `EvmContext` is created from the interpreter

**Files changed:** `crates/revmc-builtins/src/ir.rs`, `crates/revmc-builtins/src/lib.rs`, `crates/revmc-context/src/lib.rs`, `crates/revmc/src/compiler/translate.rs`, `tests/state-tests/src/lib.rs`

---

## Bug 5: Linker Hardcodes `-fuse-ld=lld` on Systems Without lld Installed

**Commit:** `2cc807b3`

### Root Cause

In `crates/revmc/src/linker.rs`, the AOT compilation linking step invokes the system C compiler (clang/gcc) to link `.o` object files into `.so` shared libraries. The original implementation unconditionally added `-fuse-ld=lld` on non-macOS systems:

```rust
} else if !cfg!(target_vendor = "apple") {
    cmd.arg("-fuse-ld=lld");
}
```

If the system doesn't have `lld` (LLVM's linker) installed, the link command fails outright, breaking the entire AOT compilation pipeline.

### Trigger Scenario

On a Linux server with only GCC installed (using default `ld`/`gold`) and no LLVM/lld:

1. revmc compiles contract bytecode to `.o` file (succeeds)
2. Invokes `clang -shared -fuse-ld=lld ...` for linking
3. clang errors: `error: invalid linker name in argument '-fuse-ld=lld'` or `lld: command not found`
4. AOT compilation fails entirely, cache cannot be used

### Fix

Added a `has_lld()` detection function using `OnceLock` to cache the result (runs only once), checking whether `lld --version` succeeds. Only adds `-fuse-ld=lld` when lld is actually available:

```rust
} else if !cfg!(target_vendor = "apple") && has_lld() {
    cmd.arg("-fuse-ld=lld");
}
```

**Files changed:** `crates/revmc/src/linker.rs`

---

## Impact Summary

| Bug | Severity | Scope | Symptom |
|-----|----------|-------|---------|
| resume_at not persisted | **Critical** | All JIT execution with CALL/CREATE | Incorrect results, extreme performance degradation |
| Gas calculation mismatch | **High** | CALL-family instructions post-Berlin | gas_used mismatch, boundary OOG divergence |
| i256 alias/speculatable | **High** | All JIT compilation at -O2/-O3 | Silent computation errors |
| CODECOPY segfault | **Critical** | All AOT cache mode | Process crash (SIGSEGV) |
| lld hardcoded | **Medium** | Linux systems without lld | AOT compilation failure |
