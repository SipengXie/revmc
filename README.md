# revmc

Experimental [JIT] and [AOT] compiler for the [Ethereum Virtual Machine][EVM].

The compiler implementation is abstracted over an intermediate representation backend. It performs very well, as demonstrated below from our criterion benchmarks, and exposes an intuitive API via Revm.

![image](https://github.com/paradigmxyz/revmc/assets/17802178/96adf64b-8513-469d-925d-4f8d902e4e0a)

This repository hosts two backend implementations:
- [LLVM] ([`revmc-llvm`]): main backend with full test coverage;
- [Cranelift] ([`revmc-cranelift`]); currently not functional due to missing `i256` support in Cranelift. This will likely require a custom fork of Cranelift.

[JIT]: https://en.wikipedia.org/wiki/Just-in-time_compilation
[AOT]: https://en.wikipedia.org/wiki/Ahead-of-time_compilation
[EVM]: https://ethereum.org/en/developers/docs/evm/
[LLVM]: https://llvm.org/
[`revmc-llvm`]: /crates/revmc-llvm
[Cranelift]: https://cranelift.dev/
[`revmc-cranelift`]: /crates/revmc-cranelift

## Requirements

- Latest stable Rust version

### LLVM backend

- Linux or macOS, Windows is not supported
- LLVM 18
  - On Debian-based Linux distros: see [apt.llvm.org](https://apt.llvm.org/)
  - On Arch-based Linux distros: `pacman -S llvm`
  - On macOS: `brew install llvm@18`
  - The following environment variables may be required:
    ```bash
    prefix=$(llvm-config --prefix)
    # or
    #prefix=$(llvm-config-18 --prefix)
    # on macOS:
    #prefix=$(brew --prefix llvm@18)
    export LLVM_SYS_180_PREFIX=$prefix
    ```

## Usage

The compiler is implemented as a library and can be used as such through the `revmc` crate.

A minimal runtime is required to run AOT-compiled bytecodes. A default runtime implementation is
provided through symbols exported in the `revmc-builtins` crate and must be exported in the final
binary. This can be achieved with the following build script:
```rust,ignore
fn main() {
    revmc_build::emit();
}
```

You can check out the [examples](/examples) directory for example usage.

## AOT Compilation Tools

### compile_hex — Compile contracts from bytecode hex

Compile one or more EVM contracts from raw bytecode hex into AOT cache (`.so` files). Supports batch input and skeleton grouping.

```bash
cargo run -p revmc-examples-runner --bin compile_hex --release -- [OPTIONS]
```

**Input modes** (mutually exclusive):

| Flag | Description |
|------|-------------|
| `--bytecode <HEX>` | Single bytecode hex string (with or without `0x` prefix) |
| `--file <PATH>` | File with one bytecode hex per line (`#` comments allowed) |
| `--stdin` | Read bytecode hex lines from stdin |

**Options:**

| Flag | Default | Description |
|------|---------|-------------|
| `--cache-dir <DIR>` | (required) | Output directory for compiled `.o` and `.so` files |
| `--skeleton` | `false` | Enable skeleton grouping: structurally identical contracts (same opcode sequence, different PUSH immediates) share one `.so` |
| `--opt-level <0-3>` | `3` | LLVM optimization level |
| `--threads <N>` | auto | Compilation parallelism |

**Examples:**

```bash
# Single contract
compile_hex --cache-dir ./revmc_cache --bytecode 6080604052348015600e57600080fd5b50

# Batch from file
compile_hex --cache-dir ./revmc_cache --file contracts.hex --skeleton --threads 16

# Pipe from cast (Foundry)
cast code 0xYourContract --rpc-url $RPC | compile_hex --cache-dir ./revmc_cache --stdin
```

**Output format:**

- Per-hash mode: `{code_hash}__prague__o3.so` (one per contract)
- Skeleton mode: `skel_{skeleton_hash}__prague__o3.so` (one per group) + `skeleton_registry.bin`

Skeleton compilation may fail for contracts where variant PUSH values are used as jump targets. In that case, the tool automatically falls back to per-hash compilation for those contracts.

Resume is supported: re-running the same command skips already-cached contracts.

### precompile — Batch compile from block snapshots

Scans `bench_data` block state files, extracts all unique contract bytecodes, and compiles them to the AOT cache.

```bash
cargo run -p revmc-examples-runner --bin precompile --release -- \
  --cache-dir ./revmc_cache --start 38004930 --count 10 --step 1000 --threads 16
```

### skeleton_precompile — Skeleton-aware batch compile from block snapshots

Same as `precompile` but with automatic skeleton grouping. Produces both per-hash `.so` (for singletons) and skeleton `.so` + `skeleton_registry.bin` (for groups of 2+ structurally identical contracts).

```bash
cargo run -p revmc-examples-runner --bin skeleton_precompile --release -- \
  --cache-dir ./revmc_cache --start 38004930 --count 9997 --threads 16
```

### Loading compiled cache in revm

On the revm side, use `AotCache` from the `revm-revmc` crate to load compiled `.so` files:

```rust
use revm_revmc::{AotCache, SkeletonRegistry};

// Load skeleton registry (optional, for skeleton-compiled contracts)
let registry = SkeletonRegistry::load(&cache_dir.join("skeleton_registry.bin"), &bytecodes).ok();

// Load all compiled functions matching the given code_hashes
let aot_cache = Arc::new(AotCache::load_from_cache(
    &cache_dir,
    registry.as_ref(),
    &code_hashes,
    &bytecodes,  // code_hash → raw bytecode mapping
));

// Create AOT-enabled EVM and execute
let mut evm = OpAotEvm::new(ctx, aot_cache.clone());
run_op_aot_handler(&mut evm)?;
```

`AotCache` tries skeleton dispatch first (one `.so` for many contracts), then falls back to per-hash `.so`, then to the interpreter.

## Credits

The initial compiler implementation was inspired by [`paradigmxyz/jitevm`](https://github.com/paradigmxyz/jitevm).

#### License

<sup>
Licensed under either of <a href="LICENSE-APACHE">Apache License, Version
2.0</a> or <a href="LICENSE-MIT">MIT license</a> at your option.
</sup>

<br>

<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in these crates by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.
</sub>
