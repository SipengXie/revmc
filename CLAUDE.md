# revmc — EVM JIT/AOT Compiler

Forked from paradigmxyz/revmc. LLVM backend, integrated with Altius revm fork for OP Stack.

## Build & Test

```bash
cargo build --workspace           # Build all crates
cargo test --workspace            # Run tests (10 known mload_overflow failures)
cargo build --release -p revmc-examples-runner  # Build benchmark binaries
```

**LLVM requirement**: LLVM 21.1 (inkwell `llvm21-1`). Installed at `/usr/lib/llvm-21/`.

## Workspace Crates

| Crate | Purpose |
|-------|---------|
| `revmc` | Core compiler: bytecode→IR translation, skeleton compilation |
| `revmc-backend` | Backend trait abstraction |
| `revmc-llvm` | LLVM backend implementation |
| `revmc-cranelift` | Cranelift backend (legacy) |
| `revmc-context` | Runtime context (`EvmContext`, FFI interface) |
| `revmc-builtins` | EVM builtin ops (host calls, gas, memory) |
| `revmc-build` | Build-time AOT compilation |
| `revmc-cli` | CLI tool |
| `revmc-cli-tests` | CLI integration tests |
| `examples/runner` | 27 benchmark/analysis binaries |

## Key Source Files

- `crates/revmc/src/compiler/translate.rs` — Main bytecode→IR translation
- `crates/revmc/src/skeleton.rs` — Skeleton-aware compilation (dedup identical opcode structures)
- `crates/revmc/src/profile.rs` — PGO branch profiling
- `crates/revmc-context/src/lib.rs` — `EvmContext` struct (FFI boundary)
- `examples/runner/src/bin_common.rs` — Shared benchmark infra

## Data & Cache Paths

- Block data: `/home/ubuntu/sipeng/bench_data/states/{block}.bin`, `txs/{block}.bin`
- AOT cache: `./jit_cache/{hash}__{spec}__{opt}.so`
- PGO cache: `./jit_cache_pgo/`

## Known Test Failures

10 `mload_overflow` tests fail (MemoryLimitOOG vs MemoryOOG) — pre-existing, unrelated to JIT work.

## Gotchas

- `bin_common.rs` is included via `#[path = "../bin_common.rs"]`, not as a proper module
- EvmContext struct size = 104 bytes; `imm_data_ptr` at offset 96 — changing layout breaks JIT
- Skeleton compilation shares one .so across contracts with identical opcode structure
- Cache spec tag is `prague` (ISTHMUS.into_eth_spec() = Prague)
