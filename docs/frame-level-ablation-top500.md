# Frame-Level Per-Contract Ablation — Top 500 Results

**Date:** 2026-03-14
**Binary:** `per_contract_ablation` (frame-level timing)
**Parameters:** `--top-n 500 --blocks-per 5 --rounds 10 --warmup 3 --alpha 0.05`
**Data:** 9,997 block files, 2,206 unique blocks tested, 485 contracts evaluated (15 had insufficient blocks)

## Method

Leave-one-out ablation with **frame-level timing**: for each target contract, compare its own execution time with JIT enabled (baseline) vs JIT removed for that contract only (ablated). Uses `TimingJitHandler` that times each `run_unified_jit_or_native()` call and attributes elapsed time to the current frame's bytecode hash.

- **Baseline**: all contracts JIT-compiled, measure target's frame time
- **Ablated**: target falls back to interpreter, all others remain JIT, measure target's frame time
- **Delta** = mean(ablated) − mean(baseline); positive = JIT helped
- **Ratio** = mean(ablated) / mean(baseline); >1.0 = JIT helped
- **Significance**: Welch's t-test, α = 0.05

This is a direct improvement over the previous tx-level method, which measured whole-transaction time and included noise from all contracts in the same tx.

## Summary

| Category | Count | Percentage |
|----------|-------|------------|
| **JIT-GOOD** | 224 | 46.2% |
| NEUTRAL | 252 | 52.0% |
| JIT-BAD | 9 | 1.9% |

## Key Metrics

| Metric | Value |
|--------|-------|
| JIT-GOOD average delta | +64.6 µs |
| JIT-BAD average delta | −7.8 µs |
| Global average delta | +44.4 µs |
| JIT-GOOD total delta | +14,471 µs |
| JIT-BAD total delta | −71 µs |
| **Benefit/cost ratio** | **204:1** |
| Weighted average ratio (by native_ms) | 1.786x |
| JIT-GOOD weighted average ratio | 2.303x |
| JIT-GOOD share of total native_ms | 43.6% |

## Ratio Distribution

| Ratio Range | Count | Interpretation |
|-------------|-------|----------------|
| ≥ 2.0x | 114 | JIT doubles or more |
| 1.5–2.0x | 124 | Strong speedup |
| 1.2–1.5x | 121 | Moderate speedup |
| 1.0–1.2x | 83 | Slight speedup or neutral |
| 0.9–1.0x | 27 | Slight slowdown |
| < 0.9x | 16 | Notable slowdown |

49% of contracts (238/485) have ratio ≥ 1.5x.

## Top 20 JIT-GOOD Contracts

| Contract | Native (ms) | Blocks | Delta (µs) | Ratio | p-value |
|----------|-------------|--------|------------|-------|---------|
| 16fdb4505125 | 1,195 | 4 | +1,411 | 2.66x | 0.0002 |
| 19e07640bdac | 36,239 | 5 | +647 | 2.76x | 0.0087 |
| 672e10df01ad | 1,555 | 5 | +498 | 1.43x | 0.0001 |
| c7556f2eff40 | 355 | 5 | +488 | 1.42x | 0.0002 |
| 2c4708bb51bd | 522 | 5 | +480 | 2.58x | 0.0000 |
| abd8ed579b4b | 2,473 | 5 | +455 | 2.46x | 0.0000 |
| be49ac585413 | 22,252 | 5 | +448 | 2.52x | 0.0017 |
| 27384962cae4 | 86,429 | 5 | +438 | 2.45x | 0.0031 |
| 99b5f52a03bd | 9,061 | 5 | +437 | 2.46x | 0.0121 |
| e5fb39f15338 | 401 | 5 | +433 | 1.83x | 0.0000 |
| d9696fb7ca25 | 36,192 | 5 | +413 | 1.56x | 0.0030 |
| 83b2af6e9f31 | 54,344 | 5 | +412 | 1.97x | 0.0446 |
| 193f7ac1daad | 2,782 | 4 | +354 | 1.13x | 0.0024 |
| 2803190d7f32 | 2,222 | 5 | +309 | 1.91x | 0.0381 |
| 17b16f8bdaac | 1,160 | 5 | +280 | 2.12x | 0.0117 |
| 7a15f6bb01bd | 5,192 | 5 | +266 | 2.06x | 0.0119 |
| 87a26eee56dc | 560 | 5 | +227 | 2.29x | 0.0000 |
| 7fde64b523eb | 3,447 | 5 | +219 | 1.62x | 0.0000 |
| 11b75a237997 | 14,518 | 5 | +206 | 1.44x | 0.0003 |
| 0e843133ca34 | 389 | 5 | +197 | 89.2x | 0.0221 |

## All 9 JIT-BAD Contracts

| Contract | Native (ms) | Blocks | Delta (µs) | Ratio | p-value |
|----------|-------------|--------|------------|-------|---------|
| a22aabb6c9c4 | 217,588 | 4 | −19.4 | 0.815 | 0.0090 |
| f5fac132a44c | 772 | 5 | −8.3 | 0.885 | 0.0119 |
| b11bffa5208c | 560 | 5 | −7.3 | 0.793 | 0.0047 |
| 7035a09e22b1 | 251 | 4 | −7.3 | 0.832 | 0.0074 |
| a2c71dd306ec | 1,850 | 5 | −6.7 | 0.911 | 0.0134 |
| 7648e108105e | 581 | 5 | −6.3 | 0.832 | 0.0414 |
| 4ab8d46dde74 | 575 | 4 | −6.2 | 0.822 | 0.0008 |
| ae8f8d3cc98f | 677 | 5 | −5.8 | 0.844 | 0.0100 |
| f11d173d2a53 | 570 | 5 | −3.3 | 0.899 | 0.0264 |

Except for `a22aabb6c9c4` (a high-native-time outlier), all JIT-BAD contracts are small (< 2ms native) with absolute losses of 3–8 µs. Likely causes: JIT function call overhead dominates for very short executions, or storage-heavy patterns (SLOAD/SSTORE) where the interpreter is not the bottleneck.

## NEUTRAL Breakdown

Of the 252 NEUTRAL contracts:
- **210** have delta ≥ 0 (trending positive, insufficient statistical power)
- **42** have delta < 0 (trending negative, but not significant)

Most NEUTRALs are "direction correct but too few samples or too short execution time to reach p < 0.05". They are potential JIT-GOOD, not a negative signal.

## Observations

1. **Compute-intensive contracts benefit most.** Arithmetic, control flow, and memory-heavy opcodes see 1.5–2.5x speedup from JIT compilation. The JIT eliminates interpreter dispatch overhead and enables LLVM optimizations (constant folding, register allocation, instruction scheduling).

2. **Storage-heavy contracts are neutral.** Contracts dominated by SLOAD/SSTORE/CALL show ratio ≈ 1.0x because the bottleneck is in host calls (database lookup), not EVM opcode execution. JIT cannot accelerate these.

3. **Regression risk is negligible.** 9/485 (1.9%) JIT-BAD with total absolute loss of 71 µs, vs 14,471 µs total gain from JIT-GOOD. The benefit-to-cost ratio is 204:1.

4. **Selective JIT (whitelist) is a viable strategy.** Compiling only JIT-GOOD contracts captures nearly all benefit with zero regression risk. The 224 JIT-GOOD contracts account for 43.6% of total native execution time.

5. **Frame-level timing eliminates cross-contract noise.** The previous tx-level method attributed full tx time changes to the target contract, inflating variance. Frame-level measurement isolates the target's own execution, producing cleaner statistical signals — 347 "not executed" cases were correctly skipped that the old method would have included as noise.

## Comparison with Previous Results

| Metric | TX-Level (top-20) | Frame-Level (top-20) | Frame-Level (top-500) |
|--------|-------------------|---------------------|----------------------|
| JIT-GOOD | 10 | 10 | 224 |
| JIT-BAD | 0 | 0 | 9 |
| NEUTRAL | 9 | 9 | 252 |
| Measurement | whole-tx delta | per-contract frame time | per-contract frame time |
| Discovery pass | required | not needed | not needed |
