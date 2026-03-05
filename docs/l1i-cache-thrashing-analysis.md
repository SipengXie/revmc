# JIT 全块回放变慢的根因：L1i Cache Thrashing

## 问题

在隔离模式（isolated snapshot）下，JIT 比 native interpreter 快 1.68×。但在真实的全块回放（full-block replay）中，JIT 反而比 native 慢 32-47%。

以 block 38004930 的 tx185 为例：

| 测量模式 | Native µs | JIT µs | Speedup |
|---------|-----------|--------|---------|
| Isolated (单 tx on snapshot) | 84.3 | **50.2** | 1.68× |
| Prefix (replay 0..185) | 108.6 | **148.6** | 0.73× |
| Full-block (replay all 227 txs) | 107.5 | **157.4** | 0.68× |

## 排除的假说

### 假说 1: 测量顺序偏置 — 已排除

Classify-Style (native→jit): JIT 148.0µs
Classify-Reversed (jit→native): JIT 149.8µs

顺序反转后结果几乎不变。

### 假说 2: `Instant::now()` per-tx 采样开销 — 已排除

Full-Block per-tx timing: JIT 148.0µs
Full-Block target-only timing: JIT 147.7µs

只对目标 tx 计时结果相同，说明每笔 tx 的 clock_gettime 开销可忽略。

### 假说 3: Snapshot 构建差异 — 已排除

`snapshot_before_tx()` 回放 184 txs 后 clone DB，与 full-block 中 tx185 执行前的 DB 状态等价。两种方式产生的 pre-tx 状态完全一致。

### 假说 4: Branch misprediction — 已排除

| 模式 | JIT branch-miss% | Native branch-miss% |
|------|-----------------|---------------------|
| Prefix | 2.74% | 4.33% |

JIT 的分支预测率反而**更好**（直接分支 vs interpreter 的间接 dispatch），branch misprediction 不是根因。

## 确认的根因：L1 指令缓存抖动

### perf stat 证据

Machine: Intel Xeon Platinum 8488C, L1i = 128KiB (4 instances), L1d = 192KiB

使用 `icache_experiment` 工具，分别在 isolated / prefix / fullblock 模式下运行 50 轮，perf stat 测量 L1-icache-load-misses：

| 模式 | L1i cache misses | Instructions | L1i miss / 1K insns |
|------|-----------------|--------------|---------------------|
| isolated_jit | 3.85M | 350M | 11.0 |
| isolated_native | 3.53M | 358M | 9.8 |
| prefix_jit | **160M** | 13.6B | **11.8** |
| prefix_native | 49.6M | 16.3B | **3.0** |
| fullblock_jit | **203M** | 16.7B | **12.1** |
| fullblock_native | 60.3M | 20.1B | **3.0** |

关键发现：
- **JIT 在全块模式下产生 3.4× 于 native 的 L1i cache miss**（203M vs 60M）
- Native 的 L1i miss rate 从隔离模式的 9.8/1K **下降**到全块模式的 3.0/1K（interpreter loop 被充分预热）
- JIT 的 L1i miss rate 始终 ~12/1K，不随模式变化（每个合约的编译函数都不同，无法共享 L1i）
- JIT 执行的总指令数比 native 少 20%，但 L1i miss 导致流水线停顿，抵消了指令减少的收益

### 机制解释

```
Interpreter 代码布局:
┌─────────────────────────────┐
│ dispatch loop (~5-10KB)     │  ← 所有 227 笔交易共享同一段代码
│ 始终留在 L1i (128KB)        │     循环利用率极高
└─────────────────────────────┘

JIT 代码布局:
┌─────────┐┌─────────┐┌─────────┐  ...  ┌─────────┐
│ fn_001  ││ fn_002  ││ fn_003  │  ...  │ fn_304  │
│ ~2-8KB  ││ ~2-8KB  ││ ~2-8KB  │  ...  │ ~2-8KB  │
└─────────┘└─────────┘└─────────┘  ...  └─────────┘
304 个编译函数 ≈ 600KB-2.4MB >> L1i 128KB
```

- 每笔交易调用不同的编译函数（不同合约地址 → 不同 bytecode hash → 不同 JIT 函数）
- 304 个 JIT 函数的总代码量远超 L1i 容量
- 每个 frame 进入 JIT 时，该函数几乎必定已被其他合约的函数 evict
- Interpreter 的 dispatch loop 只有一份，所有交易共享，始终热在 L1i 中

### 为什么隔离模式下 JIT 快？

隔离模式只运行 1 笔交易（1-4 个 frame），对应的 JIT 函数仅几 KB，完美 fit in L1i。连续 50 轮运行同一函数，L1i 命中率极高。

## 额外发现

JIT 的 IPC（instructions per cycle）在全块模式下略高于 native（2.88 vs 2.68），说明当 JIT 代码确实在 L1i 中时，执行效率优于 interpreter。瓶颈纯粹在于**代码搬运成本**。

## 优化方向

### P0: Selective JIT（选择性编译）
只对热门/重度合约启用 JIT，轻量交易走 interpreter。这是最直接有效的缓解方式。

### P1: Code layout optimization
将高频调用的 JIT 函数编译到相邻内存区域，减少 L1i 冲突。需要 profiling 数据指导布局。

### P2: Tiered compilation
- Tier 0: Interpreter（所有合约默认）
- Tier 1: 简单 JIT（仅内联算术/栈操作，代码量小）
- Tier 2: 完整 JIT（当合约被调用 N 次后升级）

### P3: JIT code size reduction
减小每个 JIT 函数的代码体积，让更多函数同时 fit in L1i。

## 复现方法

```bash
# 5 种口径对比
cargo run -p revmc-examples-runner --bin single_tx_bench --release -- \
  --block 38004930 --tx-index 185 --rounds 21 --warmup 5

# L1i cache miss 对比 (perf stat)
cargo build -p revmc-examples-runner --bin icache_experiment --release
perf stat -e L1-icache-load-misses,instructions,cycles \
  target/release/icache_experiment --tx-index 185 --rounds 50 --mode isolated_jit
perf stat -e L1-icache-load-misses,instructions,cycles \
  target/release/icache_experiment --tx-index 185 --rounds 50 --mode fullblock_jit
```

## 相关文件

- `examples/runner/src/bin/single_tx_bench.rs` — 5 种测量口径对比工具
- `examples/runner/src/bin/icache_experiment.rs` — L1i perf stat 专用实验
- `docs/jit-performance-analysis.md` — 整体 JIT 性能分析
