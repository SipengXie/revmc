# EMSE Compile Realignment — Implementation Brief

**Context.** EuroSys #2146 rebuttal, 3-day deadline. The paper's stated skeleton boundary is
EMSE-Style (strip PUSH immediates + Solidity CBOR metadata + trailing zeros) but ALL compile/
runtime numbers were measured under the old boundary (PUSH-immediates-only). We are rerunning
the full evaluation under the EMSE boundary on this machine (c5.4xlarge, rustc 1.91 —
`rustup` toolchain already installed — LLVM 21.1.8, 8 workers). This brief contains the
settled design; do not re-litigate it, but flag anything that turns out unsound in code.

Companion planning state lives in the paper repo:
`/home/ubuntu/sipeng/skelot-paper/.planning/plan.md` and `findings.md`.

## What to build (4 changes + validation)

### 1. CODESIZE runtime-indirect (small, do first)
`crates/revmc/src/compiler/translate.rs:796-805`: replace the compile-time
`self.bcx.uconst(self.isize_type, self.bytecode.code.len() as u64)` with a load of
`EvmContext.bytecode_len` (field exists at offset 88, asserted in revmc-context/src/lib.rs:66).
Exact field-load template: the VARIANT_PUSH path at translate.rs:1012-1019
(`get_field(&mut self.bcx, self.ecx, mem::offset_of!(EvmContext<'_>, bytecode_len), ...)`
then `self.bcx.load(self.isize_type, ...)`). The builtin `__revmc_builtin_codesize`
(revmc-builtins/src/lib.rs:254) is the identity function on `usize` — keep the call, only the
argument source changes. Delete the `TODO(emse-compile)` comment. Per-hash compilation is
unaffected semantically (there `ctx.bytecode_len == code.len()`). CODECOPY already reads the
original member bytecode from EvmContext (commit 6eb3a0e8) — no change.

### 2. EMSE boundary in grouping / variance / tables
Under EMSE grouping, members of one family can have different raw lengths and different
metadata bytes, so `extract_push_immediates` over FULL bytecodes yields mismatched PUSH
counts and trips the assert in `analyze_skeleton_group`. Rule: **the caller strips first**.
- Move `strip_emse_suffixes` + `extract_emse_skeleton` (+ CBOR helpers) from
  `examples/runner/src/skelot_common.rs:159-227` into `crates/revmc/src/skeleton.rs` (or a
  sibling module) so core code can use them; re-export / delegate from skelot_common to keep
  the 30+ runner binaries compiling.
- Family key: `hash(extract_emse_skeleton(code))`. Variance input:
  `members.map(strip_emse_suffixes)`. Data-table input: `strip_emse_suffixes(member)`.
  Compile input: `strip_emse_suffixes(reference).to_vec()`.
- While in `skeleton_precompile.rs:289`: grouping is `HashMap<u64, ...>` keyed by hash with
  no byte-equality verification (reviewer B asked exactly this). Add full skeleton-byte
  equality verification within each hash bucket (split colliding groups). Cost is negligible;
  it closes a real (≈1e-11) soundness hole and gives us a clean rebuttal answer.

### 3. Admission guards (this is reviewer B1's concern made real — log everything)
Guard at family-formation time; rejected members fall back to per-hash compile. Report counts.
- **g1 fallthrough guard (skeleton-level):** if the stripped code region's physical end can
  be reached and fallen through (last instruction not one of STOP/RETURN/REVERT/INVALID/
  JUMP/SELFDESTRUCT — conservative: judge the physical last instruction, no reachability
  analysis), reject the whole skeleton from EMSE sharing (its members compile per-hash or
  under the old boundary). Note: stripped TRAILING ZEROS alone are semantically STOP (0x00),
  so a region ending exactly where only zeros were stripped is safe iff falling through into
  zeros; the conservative physical-end rule already covers this — do not special-case unless
  rejection counts come back high.
- **g2 stripped-region JUMPDEST guard (member-level):** if a member's stripped suffix
  contains a byte 0x5b that the ORIGINAL full bytecode's jump analysis marks as a valid
  JUMPDEST (i.e. not inside a PUSH immediate — reuse revm's LegacyAnalyzed jump table, or
  replicate the two-pass scan in `skeleton.rs::skip_logic_push_mask`), reject that member.
- Emit per-corpus totals: skeletons rejected by g1, members rejected by g2, resulting
  realized family count. These numbers go into the rebuttal verbatim.

### 3b. All-variant must work under the EMSE boundary too
The paper-repo plan adds a full-corpus `ForceAllVariant` compile and a four-mode block
replay (interpreter / Naive / SkelOT-EMSE / all-variant-EMSE). Your boundary adaptation in
§2 should make `ClassifyMode::ForceAllVariant` operate over the same stripped regions with
zero extra work — just confirm both modes share the stripped-input path and that the
compile driver can emit all-variant artifacts to a separate cache dir in EMSE mode.

### 4. Tooling wire-up
The stage-B compile driver is what produced `compile_bench.json` (see paper repo
`scripts/run_stage_b_compile.sh` for the exact invocation — likely `skeleton_precompile` /
the compile path in `skeleton_vs_perhash_bench`). Add `--skeleton-mode {current,emse}`
(same flag pattern already exists in `push_variance.rs` / `skeleton_deployment_span.rs`)
through the compile drivers, default `current` so old behavior is untouched.

## Validation bar (before any full-corpus run)
1. `cargo test -p revmc` green (add EMSE unit tests: metadata-length-differing members group
   correctly, guards fire on crafted bytecodes, table roundtrip).
2. Smoke run on a handful of Base blocks: EMSE-mode compile + execute members vs interpreter,
   byte-identical outcomes (the exec-sanity pattern).

   **Data restoration status (paper-repo session updates this line):**
   ALL corpora restored and checksum-verified: `bench_data_base` (83GB, states/txs 10,001
   files each), `bench_data_eth`, `bench_data_bsc`, plus always-local `bench_data_arb`.
   Disk at 73% (269GB free) — fine for compile outputs.
3. Known pre-existing failures: 10 mload_overflow tests (MemoryLimitOOG vs MemoryOOG) — not
   yours.

## Deliverables
1. Commits on `jit-integration` (conventional style, no Co-Authored-By).
2. Guard-rejection counts from the smoke run.
3. The exact command lines to launch the two full-corpus background runs (do NOT launch —
   the paper-repo session coordinates machine time):
   - Base: Naive per-hash (19,130 units) + SkelOT-EMSE, `--dir /home/ubuntu/sipeng/bench_data_base`
   - Arb: both modes, `--dir /home/ubuntu/sipeng/bench_data_arb` (chain arb, 2,895 units)
4. Append a short completion note to this file (what changed, test results, guard counts).

## Constraints
- Do not change per-hash path semantics; `--skeleton-mode current` must reproduce today's
  behavior bit-for-bit.
- rustc 1.91 for release builds (`cargo +1.91 build --release -p revmc-examples-runner`).
- Keep files ≤700 LOC per repo CLAUDE.md conventions.

---

## Completion note (2026-07-30)

### What changed
1. **CODESIZE runtime-indirect** — `translate.rs` CODESIZE now loads `EvmContext.bytecode_len`
   (offset 88) via the VARIANT_PUSH field-load template instead of baking in `code.len()`.
   Verified semantically neutral for per-hash: the revm fork's `bytecode_slice()` returns
   `original_byte_slice()` (skelot-revm 46dc92f, ext_bytecode.rs:207) and revmc `Bytecode::new`
   does not pad, so baked and runtime values were always equal there.
2. **Core EMSE module `crates/revmc/src/emse.rs`** — `strip_emse_suffixes` /
   `extract_emse_skeleton` / CBOR helpers moved from `skelot_common.rs` (now delegating
   re-exports); `SkeletonMode {Current,Emse}` (FromStr, used directly by clap); admission
   guards `tail_can_fall_through` (g1) and `stripped_suffix_has_jumpdest` (g2);
   `group_skeleton_families()` with full skeleton-byte equality verification inside each hash
   bucket (colliding buckets split, split-off subgroups get salted rehashes; deterministic
   member/family ordering). `extract_opcode_skeleton` lifted into `revmc::skeleton`.
   14 new unit tests cover metadata-length-differing grouping, guard firing, collision split,
   and data-table roundtrip.
3. **Drivers wired with `--skeleton-mode {current,emse}` (default `current`)** —
   `skelot_compile_bench` (the actual Stage-B driver; EMSE artifacts go to `skelot_emse/`
   cache subdir; JSON gains an optional `grouping` block with guard counts),
   `skeleton_vs_perhash_bench` (EMSE registry saved as `skeleton_registry_full_emse.bin`),
   `skeleton_precompile` (reviewer-B byte verification now on for both modes; use a separate
   `--cache-dir` per mode), `skeleton_bench` (Stage-C three-way exec sanity). All EMSE
   variance/data-table/compile inputs go through `bytecode_view` (caller strips first).
4. Duplicated local helper copies deleted across drivers; net diff −83 lines before tests.
5. **§3b (added mid-implementation) — all-variant × EMSE** — confirmed both classify
   modes share the stripped-input path: `analyze_skeleton_group_with_mode` is fed
   `bytecode_view` output everywhere (caller-strips-first is uniform), so
   `ForceAllVariant`'s skip-logic mask is computed over the same stripped region the
   artifact is compiled from. `skelot_compile_bench` gained
   `--classify-mode {auto,force-all-variant}`; artifacts land in per-combination cache
   subdirs (`skelot`, `skelot_emse`, `skelot_forced`, `skelot_emse_forced`) and the JSON
   `grouping` block records `classify_mode`. Smoke (Base 3 blocks, 614 bytecodes):
   27 families / 485 per-hash, g1 rejected 3 skeletons (6 members), g2 rejected 7,
   0 fallbacks, artifacts under `skelot_emse_forced/`.
   **Deliberately not done:** `skelot_ablation_bench` EMSE wiring — its family input is
   the Stage-A `skeleton_families.json` generated under the OLD boundary; adding
   `--skeleton-mode emse` there before the paper repo regenerates an EMSE family file
   would silently mix boundaries in the four-mode replay. Wire it when that file exists
   (the compile side it calls — `analyze_skeleton_group_with_mode` +
   `compile_skeleton_group` — already accepts stripped inputs).

### Test results
- `cargo test -p revmc`: 618 passed, 10 failed — exactly the known `mload_overflow`
  MemoryLimitOOG/MemoryOOG failures listed in this brief (pre-existing, unrelated).
- `cargo test -p revmc-examples-runner`: all green (incl. push_variance 10/10, v1_schema 14/14).
- `cargo +1.91 build --release -p revmc-examples-runner`: clean. (Env fix: `libpolly-21-dev`
  installed — llvm-21-dev 21.1.8 apt packaging no longer ships `libPolly.a`.)
- Smoke, block 38004930 (`skeleton_bench`, 5 rounds):
  - `current`: 11 families / 50 members — identical to the historical grouping; 228/228 tx
    match across native/per-hash/skeleton, gas 30886502 EQUAL, PASS. Confirms bit-for-bit
    behavior preservation *and* CODESIZE-change neutrality on existing paths.
  - `emse`: 16 families / 61 members; 228/228 tx match, gas 30886502 EQUAL, PASS.
    Compile delta: strategy A 96.2s vs strategy B 32.9s (2.9x on the shared slice).

### Guard counts (smoke, Base 20 blocks, 1718 compilable bytecodes, 16 threads)
- Realized EMSE families: **70** (507 members); per-hash units: **1211**; fallback: 0.
- **g1 rejected 16 skeletons (51 members)**; **g2 rejected 18 members**;
  hash-collision splits: 0.
- JSON: `/tmp/emse_smoke/compile_bench_emse.json` (`records[].grouping`).
- All-variant × EMSE (3 blocks, 614 bytecodes): 27 families / 485 per-hash,
  g1 3 skeletons (6 members), g2 7 members, 0 fallbacks
  (`/tmp/emse_smoke/compile_bench_emse_av.json`).

### Full-corpus launch commands (NOT launched — paper-repo session coordinates machine time)
Machine has 16 cores; user directed full thread usage (was 8 in the original Stage-B wrapper —
per-hash and SkelOT-EMSE rows stay comparable as both use 16).

Base (10K blocks, 19,130 units, per-hash + SkelOT-EMSE):
```bash
cd /home/ubuntu/sipeng/skelot/revmc
target/release/skelot_compile_bench \
  --bench-dir /home/ubuntu/sipeng/bench_data_base \
  --start-block 38004930 --block-count 10000 \
  --mode both --skeleton-mode emse \
  --scope full_10k_emse \
  --cache-dir /home/ubuntu/sipeng/skelot-paper/caches/stage_b_emse_base \
  --threads 16 \
  --out /home/ubuntu/sipeng/skelot-paper/data/compile_bench_emse_base.json
```

Arb (10K blocks 458863320..458873319, 2,895 units, both modes):
```bash
cd /home/ubuntu/sipeng/skelot/revmc
target/release/skelot_compile_bench \
  --bench-dir /home/ubuntu/sipeng/bench_data_arb \
  --start-block 458863320 --block-count 10000 \
  --mode both --skeleton-mode emse \
  --scope full_10k_emse_arb \
  --cache-dir /home/ubuntu/sipeng/skelot-paper/caches/stage_b_emse_arb \
  --threads 16 \
  --out /home/ubuntu/sipeng/skelot-paper/data/compile_bench_emse_arb.json
```
(`--mode both` produces the Naive per-hash row and the SkelOT row in one run; the SkelOT row
carries the EMSE `grouping` guard counts for the rebuttal.)

## Addendum (paper session, 2026-07-30): same-family nested-call count for C1 rebuttal

Reviewer C asks whether nested same-family calls (same artifact twice on the call
stack, two different tables) are actually exercised by replay. We want a COUNT from
the D2 full replay: how many times a frame executes while an ancestor frame (and
separately: its direct parent) belongs to the same EMSE family, over all 10K blocks.

HARD CONSTRAINT (user): the counting must not perturb the timed replay's timing.
Preference order:
1. Offline post-processing — if the replay already emits a per-frame record with call
   ancestry (or depth + ordering sufficient to reconstruct the frame tree) plus
   code_hash, derive the count entirely offline; zero runtime change.
2. If ancestry is not reconstructible from existing output, count only in a
   NON-timed arm (exec_sanity or a separate untimed pass) — never in the timed path.

Deliverables: (a) same-family ancestor-descendant frame count, (b) same-family
direct parent-child count, 10K-block Base replay, EMSE families.

### Result (revmc session, 2026-07-30)

Option 1 (offline) was not possible: no existing replay output records frame
ancestry (frame_bench tracks a frame stack internally but emits only aggregates).
Implemented option 2 as a standalone, fully untimed tool —
`nested_family_count` (commit 81740807): interpreter-only replay with a
frame-stack hook (pattern from frame_bench's InstrumentedHandler), EMSE family
map from the shared verified grouping over the same 10K corpus, deposits
skipped per the exec-sanity convention. No timed path was touched.

Base 10K (blocks 38004930..38014929, EMSE families, 16 threads, 372s replay):

| metric | count |
|---|---|
| blocks replayed | 10,000 |
| txs replayed | 3,510,205 |
| total call frames | 50,333,517 |
| frames in EMSE families | 29,428,441 (58.5%) |
| **(a) same-family ancestor-descendant** | **3,061,778** (10.4% of family frames) |
| **(b) same-family direct parent-child** | **440** |
| families exhibiting nesting | 135 |

Top families: f44a4732 (1,389,814), 81d06818 (737,716), c35ff2d7 (508,705).
Reading for the rebuttal: the reviewer's scenario is not an edge case — the
same shared artifact sits above itself on the call stack ~3.06M times across
10K blocks (typically router → ... → same-family pool patterns; direct
self-recursion between family members is rare at 440). Every such frame gets
its own `EvmContext` with its own `imm_data_ptr`, which is exactly why the
per-call-context design matters at workload scale.

JSON: `/home/ubuntu/sipeng/skelot-paper/data/nested_family_count_emse_base10k.json`.
Rerun: `target/release/nested_family_count --bench-dir /home/ubuntu/sipeng/bench_data_base \
--start-block 38004930 --block-count 10000 --threads 16 --skeleton-mode emse --out <path>`.
