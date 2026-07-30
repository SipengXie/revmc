# SkelOT / Skeleton 模式实验总览

截至 2026-04-16。所有路径为绝对路径。此文件汇总 revmc 仓里所有与 skeleton-aware 编译机制相关的实验、代码源、数据产出。

---

## 仓库与工作区

- Revmc 主仓：`/home/ubuntu/sipeng/skelot/revmc`（git 受控）
- 论文工作区：`/home/ubuntu/hermes-shared-workspace/skelot-paper`（**非 git 仓**，所有数据/表/图/脚本/schema 存此）
- Bench 原始数据：`/home/ubuntu/sipeng/bench_data_base/states/{bn}.bin` + `/home/ubuntu/sipeng/bench_data_base/txs/{bn}.bin`（10K block 快照 + tx，blocks 38004930..38014929）
- 扫描缓存：`/home/ubuntu/.cache/skelot_index/bytecodes_38004930_10000.bin` + `.../tx_index_38004930_10000.bin`
- Stage E .so 缓存：`/home/ubuntu/hermes-shared-workspace/skelot-paper/data/stage_e_so_cache/cap32/{skeleton_id}/`
- Memory：`/home/ubuntu/.claude/projects/-home-ubuntu-sipeng-helios-revmc/memory/`
- 实验辅料：`/home/ubuntu/sipeng/skelot/revmc/docs/experiments/skeleton-data/`（PUSH variance log、部署时间 log）

---

## 1. Opcode-skeleton 去重可行性（早期探索）

**目的**：验证 EVM 字节码可剥离 PUSH 立即数得到 opcode 骨架，并测骨架级去重率。

**方法论**：`/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/opcode_dedup.rs` 扫 10K block 语料，按 PUSH1-PUSH32 剥离后 hash 聚类；按 raw 长度分桶。

**结论**：19,197 unique 字节码 → 11,259 unique skeleton（**1.7× 去重，41.4% 减少**）。huge 桶 (>15KB) 去重率 **64.3%**。Top skeleton：Uniswap V3 Pool 3323 份（14,788 B），ERC-20 465 份，EIP-1167 minimal proxy 342 份。

**数据**：
- `/tmp/skeleton_mapping.tsv`（19,197 行映射，早期产物；权威副本在 `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/skeleton_mapping.tsv`）
- Memory: `/home/ubuntu/.claude/projects/-home-ubuntu-sipeng-helios-revmc/memory/skeleton-dedup.md`

**相关源**：
- `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/opcode_dedup.rs`
- `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skeleton_identify.rs`
- `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skeleton_export.rs`

---

## 2. PUSH variance 分布（2026-04-14）

**目的**：证明 invariant/variant 分类不是空设——invariant 位占多数，支撑"share .so + 保留 LLVM 折叠"两全的设计合理性。

**方法论**：`/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/push_variance.rs` 跨 428 个 ≥2 成员 family（8366 字节码）逐位比较 PUSH 立即数；按桶 × variant% 与 variant-distinct-count 二维分析。

**结论**：
- **99.37% PUSH 位是 invariant**（586,950/590,650）
- 按字节 invariant 占 93.06%
- variant% 随尺寸单调降：tiny 43.6% → small 4.0% → medium 1.6% → large 0.7% → huge **0.5%**
- variant 深度：77% 只有 2-5 个 distinct 值
- "耗子屎"（近 invariant 被误标 variant）仅 0.2%，可忽略

**数据**：
- `/home/ubuntu/sipeng/skelot/revmc/docs/experiments/skeleton-data/push_variance.log`（21 KB，原 `/tmp` 迁入仓）
- Memory: `/home/ubuntu/.claude/projects/-home-ubuntu-sipeng-helios-revmc/memory/push-variance-results.md`

---

## 3. PUSH variance 时间稳定性（2026-04-14）

**目的**：把 invariant 从"5 小时窗口巧合"升级为"协议寿命常量"——反驳"窗口短，invariant 是统计偶然"的潜在 reviewer 质疑。

**方法论**：`/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skeleton_deployment_span.rs`，对 top-5 family 各取 30 成员，用 Alchemy Base mainnet RPC 二分搜 `eth_getCode(addr, block)` 找首次部署块，`eth_getBlockByNumber` 转 UTC。命令：`cargo run --release -p revmc-examples-runner --bin skeleton_deployment_span -- --top 5 --sample-per-group 30 --concurrency 12`。

**结论**：
| Rank | Members | Bytecode | Span | 起始 | 截止 |
|---|---:|---:|---:|---|---|
| 1 | 3,323 | 22,142 B Uniswap V3 Pool | **814.3 天** | 2023-08-19 | 2025-11-10 |
| 2 | 465 | 12,791 B ERC-20 | 122.3 天 | 2025-07-11 | 2025-11-10 |
| 3 | 401 | 22,962 B V3 Pool 变种 | **824.7 天** | 2023-08-04 | 2025-11-06 |
| 4 | 342 | 45 B EIP-1167 | **822.9 天** | 2023-08-10 | 2025-11-10 |
| 5 | 199 | 5,129 B | **797.1 天** | 2023-09-05 | 2025-11-10 |

**数据**：
- `/home/ubuntu/sipeng/skelot/revmc/docs/experiments/skeleton-data/skeleton_span.log`
- Memory: `/home/ubuntu/.claude/projects/-home-ubuntu-sipeng-helios-revmc/memory/push-variance-temporal-validation.md`

---

## 4. Skeleton-aware 编译实现（`translate_skeleton`）

**目的**：让 revmc 为一个 family 产一份 `.so`，invariant PUSH 走 `iconst_256` 被 LLVM 常量折叠，variant PUSH 走运行时 `imm_data_ptr` 查表。

**方法论**：四层设计
- `EvmContext` 加 `imm_data_ptr: *const u8`（offset 96，struct 104 字节）
- `SkeletonVariance` / `ImmDataTable` 抽象 + `analyze_skeleton_group_with_mode` / `build_data_table`
- `InstFlags::VARIANT_PUSH` + `apply_variance`
- PUSH translate 分支 variant 路径：`get_field(ecx, offset 96) → GEP(idx*32) → load i256`

API：`EvmCompiler::translate_skeleton(name, bytecode, spec, &variance)` + `jit_skeleton(...)`。每成员配一个 32×N 字节的 data_table。

**结论（block 38004930 单块 benchmark）**：11 skeleton 编译 vs 50 per-hash = **6.2× compile 加速**；执行 skeleton 比 native **1.16-1.28×**（per-hash 因 304 份 `.so` 的 L1i 抖动几乎打平 native）。

**关键源**：
- `/home/ubuntu/sipeng/skelot/revmc/crates/revmc/src/skeleton.rs`
- `/home/ubuntu/sipeng/skelot/revmc/crates/revmc/src/bytecode/mod.rs`（`apply_variance` 183-211）
- `/home/ubuntu/sipeng/skelot/revmc/crates/revmc/src/compiler/translate.rs`（PUSH 分支 1005-1037）
- `/home/ubuntu/sipeng/skelot/revmc/crates/revmc-context/src/lib.rs`
- `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skeleton_bench.rs`
- AOT 缓存：`/home/ubuntu/sipeng/skelot/revmc/jit_cache/skel_{hash}__{spec}__{opt}.so` + `skeleton_registry.bin`
- Memory: `/home/ubuntu/.claude/projects/-home-ubuntu-sipeng-helios-revmc/memory/skeleton-compilation.md`

---

## 5. Skeleton admission check（2026-04-09，已 ship）

**目的**：新 code_hash 出现时，安全决定能否复用既有 family 产物——仅匹配 skeleton hash 不够，必须逐位校验 invariant 常量。这让 SkelOT 能在**长跑生产系统**里增量接纳新合约而无需重扫整个语料。

**方法论**：`AdmissionDescriptor { descriptor_version, skeleton_fingerprint, variant_positions: Vec<u32>, invariant_values: Vec<(u32, [u8;32])> }`，`check_admission(candidate, descriptor) -> Result<ImmDataTable, AdmissionError>`，失败返回 `InvariantMismatch { push_ordinal, expected, actual }`。复用 `normalize_push_immediate` / `extract_push_immediates` 确保与 `build_data_table` 语义一致。

**结论**：11 个模块测试 + 1 个 crate 级集成测试（`skeleton_admission_matches_existing_data_table_semantics`）——admission 产出的 ImmDataTable 与 `build_data_table` 结果 byte-identical。

**关键源**：
- `/home/ubuntu/sipeng/skelot/revmc/crates/revmc/src/skeleton_admission.rs`
- Memory: `/home/ubuntu/.claude/projects/-home-ubuntu-sipeng-helios-revmc/memory/skeleton-admission-check.md`

---

## 6. Stage A — 数据集扫描（论文 Table 1）

**目的**：给 SkelOT 论文提供权威的 10K block 语料去重统计。

**方法论**：`/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skelot_dataset_scan.rs` 扫 `states/{38004930..38014929}.bin`。

**结论**：10K block 跑 364 秒；**19,130 字节码 → 11,189 skeleton = 41.51% 减少**（与早期 `opcode_dedup` 结果差 +0.001 pp）；huge 桶 **64.3% 精确**；Uniswap V3 Pool 3,323 成员精确。

**数据**：
- `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/dataset_scan.json`
- `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/skeleton_families.json`（11,189 家族元数据）
- `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/skeleton_mapping.tsv`
- Wrapper：`/home/ubuntu/hermes-shared-workspace/skelot-paper/scripts/run_stage_a_scan.sh`

---

## 7. Stage B — 全 10K 编译 benchmark（论文 Table 2）

**目的**：量化 SkelOT AOT 相对 per-hash 的编译时间 + `.o` 产物尺寸收益，展示生产可用性。

**方法论**：`/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skelot_compile_bench.rs`，冷 cache 两模式各跑一遍全 10K，记录 `per_unit_ms`、`per_unit_o_bytes`、`fallback_count`。并行编译（8 线程）。OOM 修复 commit `afd2dc28`：LLVM Context 由 `Box::leak` 换 `Box::into_raw`/`Box::from_raw` scoping，RSS 从 57 GB 稳到 2.2 GB。

**结论**：
| 指标 | per-hash | SkelOT | 对比 |
|---|---:|---:|---:|
| 编译单元 | 19,130 | 11,189 | **41.5% 降** |
| .o 总字节 | 5,030,149,584 (5.03 GB) | 2,257,985,392 (2.26 GB) | **55.1% 降** |
| 墙钟 | 40,937 s (11.4 h) | 18,194 s (5.1 h) | **2.25× 加速** |
| fallback | 0 | 0 | 零退路 |

`.o` 减少 55.1% 超过单元减少 41.5%，因为 skeleton family 共享机器码。

**数据**：
- `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/compile_bench.json`
- 权威冷跑日志（40,937 s 来源）：`/home/ubuntu/hermes-shared-workspace/skelot-paper/data/logs/stage_b_20260411T081602Z.log`
- Wrapper：`/home/ubuntu/hermes-shared-workspace/skelot-paper/scripts/run_stage_b_compile.sh`
- Memory: `/home/ubuntu/.claude/projects/-home-ubuntu-sipeng-helios-revmc/memory/full-10k-compile-time.md`

---

## 8. Stage C — 执行 sanity（论文 Table 3）

**目的**：block 38004930 上 native/per_hash/skelot 三路径 gas 与 result 一致性，并测执行加速。

**方法论**：`/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skeleton_bench.rs`，block 38004930（228 txs）重放三路径，rounds + warmup + Welch p 统计。

**结论**：**228/228 tx gas & result 完全相等**；native 22.0 ms / per_hash 17.1 ms / **skelot 16.6 ms**，skelot 相对 native **1.33× 加速**。

**数据**：
- `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/exec_sanity.json`
- Wrapper：`/home/ubuntu/hermes-shared-workspace/skelot-paper/scripts/run_stage_c_exec.sh`

---

## 9. Stage D — coverage budget（论文 Figure 2）

**目的**：给编译预算曲线——"只编译 top-N 覆盖 X% 执行流量，N 怎么选"。

**方法论**：`/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/overlap_analysis.rs --export-budget-curve`，以 `entrypoint_timing.bin`（每 tx 入口合约+执行时间）为输入，per-hash 和 SkelOT 两维生成 Pareto 二线曲线。

**结论**：同覆盖率下 skeleton 维所需编译单元数比 per-hash 小约 40%（与 Stage A/B 对齐）。

**数据**：
- `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/coverage_budget.json`
- Wrapper：`/home/ubuntu/hermes-shared-workspace/skelot-paper/scripts/run_stage_d_coverage.sh`
- Memory: `/home/ubuntu/.claude/projects/-home-ubuntu-sipeng-helios-revmc/memory/overlap-analysis.md`

---

## 10. Stage E — All-Variant 稻草人消融（2026-04-16 完成）

**目的**：从 skeleton 去重（Stage A/B 收益）里分离出 variant/invariant **分类机制**的独立贡献，回答"这一层分类是否 over-engineering"的潜在 reviewer 质疑。

**方法论（Task 13 per-frame 最终版）**：
- 100 家族分层抽样（5 bucket × 20 top-by-member_count，tiny 仅 10 个候选，总 85）
- 每家族三路：`per_hash` / `SkelOT (Auto)` / `all_variant (ForceAllVariant)`（SKIP_LOGIC PUSH 在 all_variant 下仍保留 Invariant，否则破坏静态跳转，是正确性约束）
- **Single-tx 快照测量**：`snapshot_before_tx(loader, rep_tx_index, skip_deposits=false)` 重建链上 pre-tx 状态（含 OP L1-attributes deposit），每 round clone snapshot 跑 1 条代表 tx
- **Per-frame 采样**：每个 target-hash frame 的 wall-ns 一条样本，一轮产 1-11 条，跨 rounds 聚合
- rounds=500, warmup=50, cap=32（从 cap=128 改来——large/huge 大家族 per_hash 爆炸到 28h 外推，cap=32 维持 cap-invariant 核心信号）
- **并行编译**：per_hash 8 线程并发，`compile_time_ms` 仍按 per-unit 累加（serial-equivalent）
- 持久化：per-family JSONL sidecar + 磁盘 .so cache
- Welch 双侧 t 检验，erf-based normal CDF via `libm::erf`

**Table E 最终数字**（`all_variant / skelot` 比值；85 家族，44 有 rep_tx）：

| Bucket | n | Compile ratio | .so ratio | Exec ratio | Welch p (median) |
|---|---:|---:|---:|---:|---:|
| tiny | 10 | 1.165 | 0.998 | 1.009 | 0.364 |
| small | 20 | 1.416 | 1.130 | 1.068 | 0.0204 |
| medium | 20 | 1.455 | 1.278 | 1.059 | 0.0583 |
| large | 20 | 1.246 | 1.256 | 1.092 | 0.000263 |
| huge | 15 | 1.174 | 1.319 | 1.088 | 0.0138 |

**结论**：
1. **执行端**：每次合约调用 (frame) 关掉分类后慢 **1-9%**，large/small/huge 统计显著（Welch p<0.05）。绝对差不大但有统计力。
2. **编译端**：all_variant 编译 **慢 17-46%**。反直觉——"少折叠"居然更慢。
3. **`.so` 体积**：all_variant 大 **0-32%**（huge 最甚）。

**反直觉机制的证据链**（Task 13 新增洞察）：
- Pearson 相关 `SkelOT num_invariant × (av_so_bytes − sk_so_bytes)` = **0.877** (n=85)
- 回归斜率：**~38 字节 `.so` 膨胀 / 每个"被 all_variant 变回 variant"的 PUSH**
- 单家族实测 (`16c6821f67e58be8` large): `.so` 171,664 → 196,240 (+14.3%)；指令数 34,509 → 40,340 (+16.9%)
- 汇编层面：SkelOT invariant PUSH = 1 条 7 字节 `movq-immediate`；ForceAllVariant 同位置 = GEP + load + store ~3 条 ~18 字节
- 机械结果：更多指令 → LLVM instruction selection / register allocation / scheduling 线性更多工作 → 编译更慢。不是因为"LLVM 做更多优化"，而是"LLVM 要 codegen 更多指令"。

**44/85 exec rep_tx 覆盖率**（结构性选择偏倚，不是 bug）：抽样按 `member_count`（部署数），但 10K block ≈ 5 小时语料，长尾家族部署多但 5h 内无调用。Huge 桶 26.7% 覆盖率最低，medium 桶 75% 最好。Paper 需声明"exec 结论覆盖 44 家族子集（主要活跃 ERC-20 / V3 swap / 借贷 router），不外推长尾部署"。

**产出清单**：
- 权威 JSON：`/home/ubuntu/hermes-shared-workspace/skelot-paper/data/ablation_allvariant.json` (128 KB, per-frame)
- Sidecar（可续跑）：`/home/ubuntu/hermes-shared-workspace/skelot-paper/data/ablation_allvariant.families.jsonl`
- v1 per-tx 备份（历史归档）：`/home/ubuntu/hermes-shared-workspace/skelot-paper/data/ablation_allvariant.families.v1.jsonl`
- Schema：`/home/ubuntu/hermes-shared-workspace/skelot-paper/data/schemas/ablation_allvariant.schema.json`
- Table E：`/home/ubuntu/hermes-shared-workspace/skelot-paper/tables/table_ablation.md` + `.tex`
- Figure E：`/home/ubuntu/hermes-shared-workspace/skelot-paper/figures/figure_ablation_compile_vs_exec.pdf`
- Run log（Task 13 per-frame full）：`/home/ubuntu/hermes-shared-workspace/skelot-paper/data/logs/stage_e_full_20260416T141939Z.log`
- .so 缓存（供反汇编/IR 分析）：`/home/ubuntu/hermes-shared-workspace/skelot-paper/data/stage_e_so_cache/cap32/{skeleton_id}/`
- Wrapper：`/home/ubuntu/hermes-shared-workspace/skelot-paper/scripts/run_stage_e_ablation.sh`
- 计划+设计+方法学：`/home/ubuntu/sipeng/skelot/revmc/.planning/plan_stage_e.md` + `.planning/design_stage_e_ablation.md` + `/home/ubuntu/hermes-shared-workspace/skelot-paper/methodology.md`

**关键源**：
- `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skelot_ablation_bench.rs`（955 LOC）
- `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/skelot_common.rs`
- `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin_common.rs`（含 `snapshot_before_tx` + `single_tx_evm` + `build_tx_index_cached`）

**Memory**：
- `/home/ubuntu/.claude/projects/-home-ubuntu-sipeng-helios-revmc/memory/project-stage-e-status.md`
- `/home/ubuntu/.claude/projects/-home-ubuntu-sipeng-helios-revmc/memory/skelot-compile-size-bloat-mechanism.md`（Task 13 新记录）
- `/home/ubuntu/.claude/projects/-home-ubuntu-sipeng-helios-revmc/memory/skelot-index-cache-default.md`

---

## 11. Skeleton_vs_perhash 基线对照

**目的**：为 Stage C/E TimingHandler 模式提供原型——测 per-hash vs skeleton 在单 block 上的 frame-level 延迟。

**源 + 数据**：
- `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skeleton_vs_perhash_bench.rs`（`TimingPerHashHandler` / `TimingSkeletonHandler` 原始定义，Stage E 复制改造）
- `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/skeleton_vs_perhash.json`（历史基线数据）

---

## 12. ORC v2（Design 完成，Deferred）

**结论要点**：ORC v2 与 MCJIT 共享 LLVM 管线，编译速度相当；价值在"管理"（增量、精确回收、替换 deprecated API）非"更快"。瓶颈是 LLVM O2 本身；skeleton dedup（6.2× compile 加速）比切 JIT 引擎更有效。AOT `.so` 缓存独立于 JIT 引擎。

**关键文档**：
- `/home/ubuntu/sipeng/skelot/revmc/docs/plans/2026-03-12-progressive-skeleton-jit-design.md`
- Memory: `/home/ubuntu/.claude/projects/-home-ubuntu-sipeng-helios-revmc/memory/orc-v2-design.md`

---

## 13. PGO branch profiling（已尝试，payoff 可忽略）

**结论**：对 revmc 做 branch profile 引导再编译 payoff 可忽略。设计保留不上生产。

**关键源/文档**：
- `/home/ubuntu/sipeng/skelot/revmc/crates/revmc/src/profile.rs`
- `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/pgo_bench.rs`
- `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/pgo_single_bench.rs`
- `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/collect_profile.rs`
- `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/compile_profile.rs`
- PGO 缓存：`/home/ubuntu/sipeng/skelot/revmc/jit_cache_pgo/`
- Memory: `/home/ubuntu/.claude/projects/-home-ubuntu-sipeng-helios-revmc/memory/pgo-profiling.md`

---

## 整体结论与论文可讲故事

SkelOT 的性能和架构价值**无法从一个单一数字刻画**，是五样独立收益的组合：

1. **Skeleton 去重（Stage A/B）**：41.5% 编译单元减、55.1% .o 减、2.25× compile 墙钟，对照 per_hash。
2. **Admission check 增量接纳**：新合约能加入已有 family 免重扫、无 admission 就退化为 batch AOT 工具。
3. **PUSH temporal 稳定性**：top Uniswap V3 Pool family 3323 成员跨 814 天 invariant 位一致——可证伪未被证伪，强于"5 小时窗口巧合"。
4. **LLVM 折叠保留**：Table E 执行 1.06-1.09×、编译 0.6-0.85× 的对比证明机制保留了 99.37% PUSH 的 folding 级联，关掉后 LLVM 反而更慢。
5. **PGO / 未来可扩展基座**：variant/invariant 框架给后续 hot-path specialization 一个结构化的划分界。

**只有同时做 skeleton 共享 + variant/invariant 分类**才能同时拿到 1 和 4。per_hash 赢 4 输 1；all_variant 赢 1 输 4；SkelOT 两者兼得。Table E 存在即证明这一层分类不是冗余。

**Paper 故事结构建议**：Stage A 定义 dedup 可行性 → Stage B 量化 dedup 收益 → Stage C 验证执行语义一致 + block 级加速 → Stage D 给编译预算曲线 → Stage E (per-frame) **证明 variant/invariant 分类是不可替代的层**，并且通过 Pearson 0.877 / 38 字节每 PUSH / 汇编差展示 compile/so 收益的机制性来源。

---

## 关键数据文件速查表

| 阶段/主题 | 绝对路径 |
|---|---|
| Stage A 扫描结果 | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/dataset_scan.json` |
| Stage A 家族元数据 | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/skeleton_families.json` |
| Stage A 成员映射 | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/skeleton_mapping.tsv` |
| Stage B 编译对比 | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/compile_bench.json` |
| Stage B 权威冷跑 log | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/logs/stage_b_20260411T081602Z.log` |
| Stage C 执行对比 | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/exec_sanity.json` |
| Stage D 预算曲线 | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/coverage_budget.json` |
| Stage E 权威数据（per-frame） | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/ablation_allvariant.json` |
| Stage E sidecar (resume) | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/ablation_allvariant.families.jsonl` |
| Stage E v1 备份（per-tx） | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/ablation_allvariant.families.v1.jsonl` |
| Stage E schema | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/schemas/ablation_allvariant.schema.json` |
| Stage E run log（权威） | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/logs/stage_e_full_20260416T141939Z.log` |
| Stage E .so 缓存（证据） | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/stage_e_so_cache/cap32/` |
| Stage E cap=128 部分数据（受阻 abort） | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/ablation_allvariant.cap128_partial.json` |
| Skeleton vs perhash 基线 | `/home/ubuntu/hermes-shared-workspace/skelot-paper/data/skeleton_vs_perhash.json` |
| PUSH variance raw log | `/home/ubuntu/sipeng/skelot/revmc/docs/experiments/skeleton-data/push_variance.log` |
| PUSH 时间稳定性 log | `/home/ubuntu/sipeng/skelot/revmc/docs/experiments/skeleton-data/skeleton_span.log` |
| 扫描缓存（复现用） | `/home/ubuntu/.cache/skelot_index/bytecodes_38004930_10000.bin` + `tx_index_38004930_10000.bin` |
| AOT 缓存（Stage B/C） | `/home/ubuntu/sipeng/skelot/revmc/jit_cache/{hash}__{spec}__{opt}.so` + `skel_{hash}__...so` + `skeleton_registry.bin` |
| PGO 缓存 | `/home/ubuntu/sipeng/skelot/revmc/jit_cache_pgo/` |

## 关键源码速查表

| 主题 | 绝对路径 |
|---|---|
| Skeleton 分析 + data table | `/home/ubuntu/sipeng/skelot/revmc/crates/revmc/src/skeleton.rs` |
| Admission check | `/home/ubuntu/sipeng/skelot/revmc/crates/revmc/src/skeleton_admission.rs` |
| Bytecode 分析 + `apply_variance` | `/home/ubuntu/sipeng/skelot/revmc/crates/revmc/src/bytecode/mod.rs` |
| Translate 入口（PUSH 分支） | `/home/ubuntu/sipeng/skelot/revmc/crates/revmc/src/compiler/translate.rs` |
| EvmContext（`imm_data_ptr`） | `/home/ubuntu/sipeng/skelot/revmc/crates/revmc-context/src/lib.rs` |
| Stage A 扫描器 | `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skelot_dataset_scan.rs` |
| Stage B 编译 bench | `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skelot_compile_bench.rs` |
| Stage C 执行 bench | `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skeleton_bench.rs` |
| Stage D 预算分析 | `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/overlap_analysis.rs` |
| Stage E 消融 bench | `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skelot_ablation_bench.rs` |
| PUSH variance 工具 | `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/push_variance.rs` |
| 部署时间跨度工具 | `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skeleton_deployment_span.rs` |
| 骨架去重工具 | `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/opcode_dedup.rs` |
| 协议识别 | `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skeleton_identify.rs` |
| 骨架映射导出 | `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skeleton_export.rs` |
| 共享 bench 工具 | `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin_common.rs` + `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/skelot_common.rs` |
| Baseline 对照 | `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/skeleton_vs_perhash_bench.rs` |
| Single-tx 原始工具 | `/home/ubuntu/sipeng/skelot/revmc/examples/runner/src/bin/single_tx_bench.rs` |

## 关键 commits（`jit-integration` 分支）

| SHA | 描述 |
|---|---|
| `9bedae86` | feat: add ClassifyMode::ForceAllVariant for Stage E ablation |
| `45a3a85c` | feat: scaffold skelot_ablation_bench binary |
| `799738a0` | feat: add Stage E compile phase (per_hash/SkelOT/all-variant) |
| `f3522de4` | feat: wire Stage E exec phase — frame-level Welch across 3 modes |
| `8cf124bc` | fix: emit null welch for empty exec samples; findings + dead-code polish |
| `019a3f3a` | feat: add per-family JSONL persistence + --resume for Stage E |
| `0f1d81e5` | perf: cache corpus bytecode + tx_index scans for Stage E |
| `77d79a5b` | refactor: lift snapshot_before_tx to bin_common for Stage E reuse |
| `e6a09f1c` | refactor: propagate rep tx_index through Stage E cap_members |
| `af02c5d7` | feat: switch Stage E exec to single-tx snapshot measurement |
| `2b16bdbf` | fix: apply Stage E dual-review findings (deposit replay + empty-sample guard + extraction) |
| `f314d452` | perf: parallelize per_hash compile in Stage E bench |
| `d5884a0b` | perf: per-frame exec sampling + persistent .so cache for Stage E |
