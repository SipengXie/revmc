# Skeleton Experiment Data (relocated from /tmp)

Historical measurement outputs that back claims in memory files and the SkelOT paper. Moved here 2026-04-16 so these artifacts survive `/tmp` cleanup.

## Files

### `push_variance.log`
- **Produced**: 2026-04-14 by `examples/runner/src/bin/push_variance.rs` on Base mainnet blocks `38004930..38014927` (10K-block corpus).
- **Reproduce**: `cargo run --release -p revmc-examples-runner --bin push_variance`.
- **Content**: raw run log — cargo warnings preface a global variance summary, per-bucket variant% breakdown, variant depth histogram, and a "耗子屎" (near-invariant misclassification) check.
- **Headline numbers** (used in `memory/push-variance-results.md`):
  - 428 skeleton families (≥2 members), 8,366 bytecodes
  - 590,650 PUSH positions total; **99.37% invariant**, 0.63% variant
  - variant% by bucket: tiny 43.6% → small 4.0% → medium 1.6% → large 0.7% → huge 0.5%
  - 77% of variant positions take 2–5 distinct values; only 0.2% are near-invariant mislabels.
- **Supports**: the invariant/variant split underlying `translate_skeleton` and `skeleton_admission`; direct input to the argument that variant-ness concentrates in tiny contracts and vanishes in huge ones.

### `skeleton_span.log`
- **Produced**: 2026-04-14 by `examples/runner/src/bin/skeleton_deployment_span.rs` against Base mainnet RPC (Alchemy).
- **Reproduce**: `cargo run --release -p revmc-examples-runner --bin skeleton_deployment_span -- --top 5 --sample-per-group 30 --concurrency 12`.
- **Content**: for each of the top-5 skeleton groups, binary-searches `eth_getCode(addr, block)` to locate each sampled member's deployment block, then converts to UTC via `eth_getBlockByNumber`.
- **Headline numbers** (used in `memory/push-variance-temporal-validation.md`):
  - Top-1 Uniswap V3 Pool (3,323 members): **814.3-day deploy span**, 2023-08-19 → 2025-11-10.
  - Top-3 Uniswap V3 Pool variant (401): 824.7 days.
  - Top-4 EIP-1167 minimal proxy (342): 822.9 days.
  - Top-5 (199 members): 797.1 days.
  - Top-2 ERC-20 template (465): 122.3 days (young template, still identical invariant set per member).
- **Supports**: upgrade of the invariant claim from "stable in a 5-hour measurement window" to "stable across 2+ years of mainnet deployments."

## Not moved

Other `/tmp/ablation_*.json`, `/tmp/stage_e_smoke.log`, `/tmp/ablation_smoke_resume.*`, etc. were intra-session dev/smoke artifacts for Stage E persistence + single-tx refactor validation and are considered ephemeral. The canonical copies live in `~/hermes-shared-workspace/skelot-paper/data/logs/` (wrapper-run logs) and `~/hermes-shared-workspace/skelot-paper/data/ablation_allvariant.{json,families.jsonl}` (latest smoke / full output).

## Authoritative Stage A/B/C/D/E paper data

Lives in the paper workspace (non-git): `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/`. See the paper workspace's own README for the catalog.
