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

## Addendum 2 (paper session, 2026-07-30): g1 upgraded to reachability-based check

User caught that g1 was rejecting 205 skeletons (903 members) whose tails are embedded
Solidity revert-string DATA — the linear scan misread data bytes as non-terminal
instructions. Fixed in `tail_can_fall_through` (crates/revmc/src/emse.rs): the region
after the LAST flow-blocking instruction (STOP/RETURN/REVERT/INVALID/JUMP/SELFDESTRUCT)
is unreachable iff it contains no boundary JUMPDEST (sequential flow blocked + no jump
landing site) — in that case the physical end cannot be reached and the skeleton is
admissible regardless of what the data looks like (truncated PUSH included). Data
containing 0x5b ('[') at a boundary, or code genuinely ending in JUMPDEST, still
rejects (the EVM itself would accept a jump there). 3 new unit tests; all 16 emse
tests green.

Effect on Base 10K EMSE grouping (grouping_stats, same shared function):
- g1: 205 skeletons/903 members → **7/17**; g2: 283 → 317 (+34 newly checked)
- units: 10,994 (42.5%) → **10,337 = 46.0% reduction**; families 594 → 783
- PRE-LAUNCH REQUIREMENT: exec_sanity must re-run over the newly-shared families
  (189 additional families) before the timed compile launch; grouping change is
  admission-semantics change.

## Addendum 3 (paper session, 2026-07-30): g2 OFF via interpreter alignment; g1 stays

User decision + core rationale (both guards were designed deliberately; this records
why their fates now differ):
- **g2 switched OFF** (`ENFORCE_G2 = false`, emse.rs): the metadata / non-metadata
  boundary IS reliably identifiable today (length trailer + CBOR whitelist check), so
  instead of filtering members whose metadata contains a boundary 0x5b, we ALIGN the
  interpreter: our skelot-revm fork must mark stripped-metadata positions as invalid
  jump targets when building the jump table (same strip function decides the boundary).
  Then "jump into metadata" faults on BOTH sides — behaviors agree, filtering is
  redundant. Effect: 317 members return to sharing; Base 10K grouping is now
  **818 families (9,913 members) + 9,217 per-hash = 10,035 units = 47.5% reduction**
  (static ceiling 10,025; only g1's 7/17 and degradations remain).
- **g1 stays ON**: the code/data boundary INSIDE the code region is NOT reliably
  identifiable (EVM has no data sections), so only the reachability proof
  (last-blocker + no-JUMPDEST) works there. 7 skeletons / 17 members rejected.
- **EOF endgame**: once EOF-style formats separate code/data/metadata at the format
  level, both guards become unnecessary. Upstream positioning: propose the metadata
  jump-mask as an OPT-IN hardening flag / EIP-direction discussion (EIP-3779/EOF are
  already headed there) — never as a semantics-changing default PR.
- **WORK FOR THIS SESSION (revmc side), required BEFORE the D2 replay:**
  1. skelot-revm fork: mask stripped-metadata positions out of the legacy jump table
     (use strip_emse_suffixes for the boundary; ~tens of lines).
  2. Add a replay counter: how many executed JUMP/JUMPI actually target the stripped
     region over the 10K blocks (expect 0 — that zero is the paper's evidence that the
     hardened semantics changes nothing on real workloads).
  3. exec_sanity re-run over the final 818-family grouping (g1-upgraded + g2-off).
  Tonight's compile launch uses this grouping (g2 off) — the fork change lands before
  any execution-side measurement.

## Addendum 4 (revmc session, 2026-07-30): full-experiment execution log

Running the hand-off brief `skelot-paper/.planning/handoff-experiment-prompt.md`.
This section accumulates per-step numbers, anchor checks, and any deviations.

### Pre-flight (done 2026-07-30 ~16:10 UTC)
- g1-reachability + g2-off finalized state committed: revmc `c74b09f7` (emse.rs
  ENFORCE_G2=false, reachability-based tail_can_fall_through, grouping_stats bin)
  + docs `b033a808`. Release binaries rebuilt AFTER these commits.
- **Grouping anchor check (grouping_stats, Base 10K, emse): ALL MATCH** —
  19,130 bytecodes; 818 multi-member families (9,913 members); 9,217 per-hash;
  10,035 units (47.5% reduction); g1 rejected 7 skeletons / 17 members;
  g2 rejected 0; 0 collision splits. Grouping wall 0.80 s.

### Step 2 — Base dual-mode full compile: LAUNCHED
- Started 2026-07-30 16:09:31 UTC, cold cache (cache-dir did not exist), 16 threads.
- Log: `skelot-paper/data/logs/stage_b_emse_base_20260730T160931Z.log`;
  out: `skelot-paper/data/compile_bench_emse_base.json`.
- Binary built at revmc `b033a808` (post-finalization), revm fork rev `46dc92f`
  (pre-mask — compile timing does not touch the interpreter jump table; the
  fork mask lands before any execution-side measurement per Addendum 3).
- Early log confirms 19,130 collected bytecodes (Naive side will compile all).
- Deviation note (recorded for transparency): during the first ~30 min of the
  Naive arm, `nice -19 -j2` cargo check/test of the revm fork ran concurrently
  (~2 low-priority threads for a few minutes against 16 compile workers,
  estimated <0.1% wall-clock effect). All heavy builds/tests are deferred to
  after the timed window; no further concurrent load will be introduced.

### Step 1 — interpreter fork changes: IN PROGRESS (code written, tests pending)
- skelot-revm working copy at `/home/ubuntu/sipeng/skelot/skelot-revm`.
- (a) `analyze_legacy` masks stripped-suffix positions (metadata + trailing
  zeros) out of the legacy jump table; boundary via a fork-local copy of
  `strip_emse_suffixes` (`crates/bytecode/src/legacy/emse.rs`), guarded by a
  cross-repo equivalence test in revmc's emse.rs (fixed sample set).
- (b) [JUMP-HIT]: `is_valid_legacy_jump` cold path (invalid target only) calls
  `emse_jump_audit::record_if_masked_hit` — counts targets that are ≥ stripped
  len AND vanilla-valid JUMPDESTs on the original bytes. Global AtomicU64,
  read/take APIs; valid-jump hot path untouched. Timed arms read the total
  AFTER the timed window; tx-hash attribution (if count ≠ 0) will use a
  separate untimed replay.
- Replay-side hydration fix (required for the mask to matter): snapshots store
  pre-analyzed jump tables, so `BinLoader::new` now re-analyzes legacy code
  from original bytes (load phase, all arms symmetric). Recorded here because
  it is a runner-side change beyond the fork — without it the stored
  (unmasked) tables would silently bypass the hardened semantics.

### INCIDENT 1 (2026-07-30 23:47 UTC): SkelOT arm abort at launch — diagnosed, fixed, relaunching
- What happened: the Base `--mode both` run finished the ENTIRE Naive arm
  (19,130 units, ~27,600 s ≈ 7.7 h wall) and then aborted (SIGABRT) at the
  very start of the SkelOT arm: `apply_variance` assert "PUSH at index 398
  is both SKIP_LOGIC and Variant". Grouping stats printed by the run still
  matched every anchor (818 / 9,217 / g1 7-17 / g2 0 / 0 splits). The
  per-hash timing detail died with the process (JSON is written at the end),
  so the Naive numbers of this attempt exist only as log progress lines.
- Root cause (two layers):
  1. One EMSE family (skel 0ec92b5d1a2b3c7b, 3 members) has 14 PUSHes whose
     values differ across members (Auto ⇒ Variant) AND feed an adjacent
     JUMP/JUMPI (backend folds them to compile-time static jumps). Members
     genuinely jump to different targets — one shared artifact cannot fold
     those jumps; the assert is correct. Old-boundary runs never saw this:
     such families only group under the EMSE boundary (metadata-differing
     variants). Diagnosed with the new `skip_logic_conflicts` bin: exactly
     1 family / 3 members / 14 PUSHes affected out of 818 families.
  2. The driver ALREADY had a per-family fallback (catch_unwind →
     per-member compilation, `fallback` counter), but the workspace builds
     with `panic = "abort"` — catch_unwind can never fire, so the intended
     degradation path was dead code and the assert killed the process.
     (A "green light that cannot turn red": the fallback had never been
     exercised under the release profile.)
- Fix (minimal, semantics-preserving, timing path untouched):
  `revmc::skeleton::variance_conflicts_static_jumps` (reference bytes +
  variance → bool) + PRE-CHECK during the untimed analysis phase in all four
  skeleton consumers (skelot_compile_bench, vs_perhash slow path,
  skeleton_bench, skeleton_precompile; skelot_ablation_bench skips such
  families). Conflicting families degrade to per-hash exactly like the
  existing `fallback` semantics; counts are logged and exported in the JSON
  `grouping` block (`static_jump_degraded_families/members`). The compiler
  assert stays (now a guarded invariant). g1/g2 logic untouched.
- Expected effect on anchors: grouping-level numbers unchanged (818 families
  etc. are pre-degradation); compile-level realized families 818 → 817,
  SkelOT units 10,035 → 10,037 (+3 members −1 family). Well inside the
  "degradations" allowance already noted in Addendum 3 (static ceiling
  10,025 vs 10,035).
- Verification: `cargo test -p revmc skeleton` 29/29 green (incl. new
  detection test); 10K smoke of the skeleton arm confirms the degradation
  fires in the analysis phase and compilation proceeds past it.
- Action: Base `--mode both` relaunched from scratch into a FRESH cache dir
  (cold start, both arms in one run for provenance); the aborted attempt's
  partial cache was discarded.
- DEFENSE-IN-DEPTH NOTE (user, 2026-07-31): the `apply_variance` assert is a
  defense that WORKED. Without it, this family would have compiled into a
  shared artifact with the reference member's jump targets baked in — the
  other two members would silently take wrong branches at runtime (a
  correctness bug far worse than a crash). The assert turned "wrong artifact"
  into "loud stop". Keep it; the pre-check makes it unreachable, the assert
  stays as the last line.
- POST-RUN INSPECTION (user request): after THIS compile run finishes and
  BEFORE launching the Arb step, disassemble and diff the 3 members of
  family 0ec92b5d1a2b3c7b to pin down the real-world origin of
  "same skeleton, different jump targets" (template-patched deploys vs
  parameterized branches). Record findings here.
- **CORRECTION FOR §6.2 (user caught, 2026-07-31): the static-jump
  degradation count is NOT zero.** Any paper text saying "members disagreeing
  on compile-time-fixed positions (e.g. static jump targets): measured zero"
  must be updated to: **1 family / 3 members (14 conflicting static-jump
  PUSHes), Base 10K, EMSE boundary** — family 0ec92b5d1a2b3c7b, the same one
  behind INCIDENT 1. The authoritative numbers land in
  `compile_bench_emse_base.json` → `grouping.static_jump_degraded_families
  / _members` when the relaunched run completes. §6.2 should report it
  alongside g1's 7 skeletons / 17 members. ("Zero" was true under the old
  boundary, where such variants never grouped in the first place.)

### Code review (2 independent Opus agents, 2026-07-31): verdict SAFE, with method notes
Scope: fork db2ea67 (jump mask + audit) and revmc c74b09f7/7b37c7be/f51f0306/dc55f295.
Strong confirmations (checked condition-by-condition by the reviewers):
- skip_logic_push_mask ≡ backend static_jump_analysis (imm>8, truncated PUSH,
  PUSH0, spec gates, same-bytes-everywhere): the panic=abort channel is closed.
- g1 reachability proof sound (JUMPI correctly non-blocking; a pre-blocker
  JUMPDEST cannot help; PUSH straddling the strip boundary is necessarily in
  the unreachable region).
- BinLoader re-analysis audited against EVERY runner binary: no execution
  path reaches stale (unmasked) jump tables. Fork/core strip implementations
  are token-identical today; mask is provably a no-op unless the CBOR
  trailer detector fires (trailing zeros cannot contain a JUMPDEST).
Accepted findings and dispositions:
1. [JUMP-HIT] had no reader → step-4 driver MUST read masked_jump_hits()
   after the interpreter arm (only pure-interpreter frames consult the
   table; a JIT-arm reading would undercount structurally). Task updated.
2. Naive arm is semantically mixed (JIT frames vanilla, interpreter-fallback
   frames hardened); three-arm comparability rests on interpreter-arm
   [JUMP-HIT]=0. Recorded as a methodology precondition.
3. Registry fast path predating the degradation filter could resurrect the
   conflicting family → RegistryEntry.static_jump_checked marker added,
   loader now refuses old files; existing old registries renamed
   *.pre-degradation-filter. Timed runs rebuild registries from scratch.
4. [FAM-ANALYSIS] via grouping_stats --time-analysis is Auto-mode and its
   data_table_seconds belongs to the exec-side registry build, not the
   compile driver; report variance_analysis_seconds as the compile-phase
   cost and label the table cost separately.
5. skelot_ablation_bench SKIPS conflicting families (vs degrade-to-per-hash
   elsewhere) — fine for its sampling purpose, but its family universe
   differs from the compile driver's by design; note when reconciling.
6. variance_conflicts_static_jumps zip length check upgraded debug_assert →
   hard assert. Fork follow-ups (comment truth + 2 missing tests: mask
   neutrality over trailing zeros, end-to-end InvalidJump+counter) queued
   for the next fork-touch window.

### Cross-chain corpus composition (rebuttal ammo, 2026-07-31)

Why does the Arb 10K corpus hold only 2,895 unique bytecodes vs Base's
19,130? Measured, not guessed — 200 random blocks per chain parsed straight
from `txs/{bn}.bin` (bincode TxBin layout; parser self-validated against the
known Base anchor of 351.0 tx/block from the full 10K replay — sample gave
351.8):

| | Base 10K | Arb 10K |
|---|---|---|
| tx per block (sampled) | 351.8 | 3.7 |
| pure transfers (empty calldata) | 2.4% | 8.5% |
| contract calls | 97.3% | 91.5% |
| approx. total txs in corpus | ~3.5 M | ~37 K |
| wall-clock span of 10K blocks | ~5.6 h (2 s/block) | ~42 min (~0.25 s/block) |

Reading: the "mostly transfers" hypothesis is ruled out — both chains are
>91% contract calls. The gap is sample-size, not ecosystem: an Arb block
carries ~95× fewer txs and 10K blocks span ~8× less wall time, so the
corpus contains ~2 orders of magnitude fewer transactions and correspondingly
fewer touched contracts. Useful for: (a) footnoting the cross-chain corpus
calibration; (b) explaining Arb's low family density / modest compile
speedup as a sampling-window property, not a method failure.
Method: inline Python parser over bincode TxBin (fields per
examples/runner/src/bin_common.rs TxBin), seed 7, 200 blocks/chain.

### Family 0ec92b5d forensics (task done before Arb launch, 2026-07-31)
Members (Base 10K), all raw_len=11,224 / stripped_len=11,171 (53-byte CBOR):
- 0x01da7045… (member A), 0x0e1168d0… (B), 0xf6ac0a06… (C)
Byte-level facts (skip_logic_conflicts forensic dump):
- 196 differing bytes inside the stripped region — by construction all in
  PUSH immediates (identical skeleton ⇒ opcode bytes identical), plus the
  53-byte metadata each.
- All 14 conflicting static-jump PUSHes are PUSH2; **B and C agree on every
  one of them — the divergence is entirely A vs (B,C)**, with regular target
  displacements (e.g. 0x26f1 vs 0x2711 = +0x20; 0x26b1 vs 0x2731 = +0x80),
  and each value lands on a JUMPDEST valid in all three (same skeleton ⇒
  same JUMPDEST set).
Reading: B and C are two deployments of the same variant (differing in other
immediates + metadata); A is a variant with genuinely different control-flow
entry points — same template, different compile-time configuration steering
several branches to different targets. A "structural false twin": byte-wise
identical skeleton, different logic. Exactly the class the static-jump
pre-check must degrade to per-member compilation; the compile run confirmed
1 family / 3 members, and §6.2 can cite this family as the concrete instance
(with the defense-in-depth note: the apply_variance assert stood behind it).

## Addendum 5 (revmc session, 2026-07-31): Step 2 COMPLETE — Base 10K dual-mode compile results

Run: attempt 2 (post-INCIDENT-1 fix), cold cache, 16 threads, binary at
f51f0306 lineage (fork rev db2ea675). Log:
`skelot-paper/data/logs/stage_b_emse_base_20260731T003143Z.log`; JSON:
`skelot-paper/data/compile_bench_emse_base.json` (both arms + grouping block).

| placeholder | value | provenance |
|---|---|---|
| [B-EMSE-UNITS] | 10,037 (817 families + 9,220 per-hash) | JSON skelot.n_units / grouping |
| [B-EMSE-RED] | 47.5% (19,130 → 10,037) | derived |
| [B-EMSE-SIZE] | 2.151 GB (Naive 5.050 GB) | total_o_bytes |
| [B-EMSE-SIZE-RED] | **57.4%** | derived |
| [B-EMSE-TIME] | 13,483.4 s (3.75 h) | skelot.total_compile_seconds |
| [B-EMSE-ABS] | 27,860.4 s (7.74 h) Naive all-19,130 | per_hash.total_compile_seconds |
| [B-EMSE-SPEEDUP] | **2.07×** cold-compile | derived |
| [FAM-ANALYSIS-S] | 0.305 s variance analysis (+0.78 s grouping, +0.234 s tables/9,910) | grouping_stats --time-analysis (standalone rerun of the same deterministic code; Auto mode; table cost is exec-side registry build, not compile) |
| [FAM-ANALYSIS-PCT] | 0.0023% of pure compile | derived |

Anchor checks: size reduction 57.4% vs ~55% reference — PASS (mildly
better); speedup 2.07× vs ~2.25× reference, band [1.8, 3.5] — PASS
(slightly under reference; known conservative bias: skeleton-arm singletons
are compiled in random order, worse tail packing than the size-sorted Naive
arm — recorded, per-unit detail in JSON allows an unbiased offline check).
Naive side compiled all 19,130 units, fallback 0; degradations exactly the
pre-announced 1 family / 3 members (static-jump); g1 7/17, g2 0, collisions 0.
Naive total 5.050 GB closely matches the old-boundary measurement (5.03 GB).

### INCIDENT 2 (2026-07-31 ~14:52 UTC): Arb attempt 1 externally terminated — cause unknown
First Arb launch (14:16 UTC) ran healthily to 400/2,895 units (~35 min),
then the whole task was killed by a task-level stop signal — not a crash
(no error, no OOM record), not initiated by this session; user confirms
they did not stop it either. Partial cache and sidecar were discarded;
attempt 2 relaunched cold at 14:54 UTC (anchor line `Collected 2895` hit
again). Only cost: ~35 min. Watch item: if attempt 2 is terminated the
same way, stop and escalate instead of relaunching.

### NOTE FOR PAPER SESSION (user, 2026-07-31): absolute numbers in prose
When citing compile results in the paper/rebuttal, always pair the ratio
with the absolute figures (e.g. "2.07× — 27,860 s vs 13,483 s wall;
57.4% — 5.050 GB vs 2.151 GB; 19,130 vs 10,037 units"), so reviewers
cannot object that only percentages are reported. All absolutes are in the
Addendum 5 table and in `compile_bench_emse_base.json`.

## Addendum 6 (revmc session, 2026-07-31): Step 3 COMPLETE — Arb 10K dual-mode compile results

Attempt 2 (attempt 1 lost to INCIDENT 2), cold cache, 16 threads, binary
with durability patches (arm flush + sidecar; timing semantics unchanged).
Log: `stage_b_emse_arb_20260731T145430Z.log`; JSON:
`compile_bench_emse_arb.json` (+ .units.jsonl sidecar).

| placeholder | value | provenance |
|---|---|---|
| [ARB-RED] | **23.1%** (2,895 → 2,227 units; 171 families + 2,056 per-hash) | JSON grouping |
| [ARB-SIZE-RED] | **26.2%** (646.0 MB → 476.9 MB) | total_o_bytes |
| [ARB-SPEEDUP] | **1.29×** cold (4,660.3 s → 3,612.0 s) | total_compile_seconds |

Anchor checks — ALL PASS: 2,895 bytecodes exact; units 2,227 inside
[2,225, 2,378] and only +2 off the static ceiling (exactly g1's 2 rejected
skeletons / 4 members re-compiled individually); reduction at the band's
upper end; guards single-digit (g1 2/4, g2 0, degradations 0, collisions 0,
fallback 0); speedup far below Base's 2.07× as pre-declared (lowest family
density — see the tx-composition note: the 10K-block Arb window holds ~37 K
txs vs Base's ~3.5 M).

## Addendum 7 (revmc session, 2026-07-31): exec_sanity over the FINAL grouping — 0 mismatches

Full-corpus paired run (`skeleton_vs_perhash_bench --only-mode both
--sample-every 1 --rounds 1 --warmup 0`, single-threaded, ~4.2 h), log
`exec_sanity_emse_base_20260731T171340Z.log`, JSON `exec_sanity_emse_base.json`.
- **Correctness anchor PASS: 0 MISMATCH lines over 10,000 blocks** (0 skipped);
  per-arm replay totals identical to the digit: 3,520,323 txs, 2,511,307
  successes, total gas 458,309,813,705 in BOTH arms.
- Paired per-frame timing (bonus): 6,486 members with enough samples —
  median speedup **1.2424×** (reference 1.17×, band [1.0, 1.5] — PASS);
  2,999 SKELETON_FASTER / 3,420 EQUIVALENT / 67 SKELETON_SLOWER.
- **[JUMP-HIT] observation: count = 4 in this (JIT-armed) run** — flagged
  not-meaningful for this arm (JIT frames bypass the table; each block is
  replayed 4×, so 4 counts may be ONE unique tx). The authoritative number
  comes from the step-4 interpreter arm (1 replay/block); if non-zero there,
  a separate untimed pass will attribute exact tx hashes per the brief
  ("non-zero is a finding, not a failure").
- Registry with fingerprints written: `data/skeleton_registry_full_emse.bin`
  (reused by the three-arm replay).

## Addendum 8 (revmc session, 2026-07-31): Step 4 COMPLETE — three-arm full replay

Three sequential single-arm runs (`--sample-every 1 --rounds 1 --warmup 0`,
registry-fed, quiet machine, ~40 min total). JSONs:
`exec_replay_{interpreter,per_hash,skeleton}_base.json`.

| placeholder | value | provenance |
|---|---|---|
| [E2E-INT] | **534.4 s** exec wall (10,000 blocks) | interpreter mode_report |
| [E2E-NAIVE] | **459.1 s** | per_hash mode_report |
| [E2E-NAIVE-COMPILE] | 27,860.4 s | Addendum 5 |
| [E2E-SK] | **450.6 s** | skeleton mode_report |
| [E2E-SK-COMPILE] | 13,483.4 s | Addendum 5 |
| [E2E-FAM] | 0.305 s variance (+0.78 s grouping) | Addendum 5 |
| [JUMP-HIT] | **1** (authoritative, interpreter arm, 1 replay/block) | interpreter log |

Anchor checks: ordering PASS (interpreter slowest); SkelOT (450.6 s) is
FASTER than Naive (459.1 s) end-to-end — no stop condition; per-frame
median speedup 1.2424× from the paired run (Addendum 7) sits in [1.0, 1.5].
Note on magnitude: E2E deltas are diluted by non-JIT time (state access,
non-family frames); the per-frame numbers are the compiled-code comparison.

Cross-arm ledger: per_hash and skeleton agree TO THE DIGIT (txs 3,520,323 /
ok 2,511,307 / gas 458,309,813,705) — the shared-artifact arm introduces
zero behavioral drift vs Naive. Interpreter vs JIT arms differ on 3,803 txs
(0.108%) and 27.3 M gas (+1 sample count): a PRE-EXISTING
interpreter-vs-JIT granularity gap (gas accounting at OOG boundaries) plus
exactly one masked-jump event; NOT introduced by this work (the two JIT
arms' exact agreement is the paper's correctness claim). Recorded for
methodology; per-tx attribution of the divergence is possible offline from
the per-arm ledgers if a reviewer asks.

[JUMP-HIT]=1: exactly one tx in 10K blocks jumps into the stripped metadata
region at a vanilla-legal JUMPDEST (the both-mode count of 4 = 1 tx × 4
replays — consistent). Attribution run in progress (untimed single-thread
pass, take_masked_jump_hits per tx); result appended below.

### [JUMP-HIT] attribution (untimed pass, 2026-07-31)
The single event: **block 38013100, tx_index 351, caller
0x2e76eb850ea488b13dab713e1893716610c826a9, a CREATE (deployment) tx**
(calldata opens with the standard Solidity constructor prologue). Reading:
- **Zero hits on deployed (runtime) code across all 10K blocks** — the
  hardened jump semantics change nothing for regular contract calls.
- The one hit is inside INITCODE execution: initcode tails embed the future
  runtime's CBOR trailer, so the boundary detector fires on the nested
  structure and this constructor happens to jump there. Deployment frames
  never participate in shared compilation (create frames run the
  interpreter in every arm), so this event is orthogonal to the SkelOT
  correctness claim and identical across arms.
- For the upstream-hardening pitch: note an initcode exemption (or scope
  the mask to runtime code) in the EIP-direction discussion.
Tool: `jump_hit_attribution` (untimed, single-threaded, per-tx
take_masked_jump_hits). Rerun: `target/release/jump_hit_attribution
--bench-dir /home/ubuntu/sipeng/bench_data_base --start-block 38004930
--block-count 10000`.
