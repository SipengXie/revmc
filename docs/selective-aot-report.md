# Selective AOT: 在指令缓存压力下优化 EVM JIT 编译策略

## 1. 摘要

revmc 是一个基于 LLVM 的 EVM AOT 编译器，将 EVM 字节码预编译为本机代码以提升执行速度。
在 OP Stack 的真实区块（block 38004930, 304 个唯一合约）上，All-AOT 模式相比纯解释器
（Native）仅获得 **1.17x** 加速。

通过硬件性能计数器（`perf stat`）分析，我们发现瓶颈在于 **L1 指令缓存（L1i）和指令
TLB（iTLB）的双重压力**：304 个 AOT 函数的 `.text` 段总计 49MB，占用 12,775 个 4KB 页，
远超 L1i（32KB/core）和 iTLB（~128 条目）的容量。

基于此发现，我们设计了 **fullblock ablation** 实验方法，在真实缓存压力下逐一评估每个合约
的 AOT 效果。结果表明：仅 14%（34/245）的合约在真实条件下受益于 AOT；去除有害合约的
**No-Bad 策略**（177 个 AOT 函数）达到 **1.21x** 加速，优于 All-AOT 的 1.17x。

| 策略 | AOT 函数数 | 执行时间 | 相对 Native |
|------|-----------|---------|------------|
| Native（解释器） | 0 | 21.10ms | 1.00x |
| Good-Only | 34 | 18.94ms | 1.11x |
| All-AOT | 304 | 18.07ms | 1.17x |
| **No-Bad** | **177** | **17.39ms** | **1.21x** |

## 2. 背景

### 2.1 revmc AOT 架构

revmc 将 EVM 字节码编译为本机代码，编译流程为：

```
EVM bytecode → LLVM IR → .o (object) → .so (shared library) → dlopen → 函数指针
```

每个唯一的合约字节码被独立编译为一个 `.so` 文件，缓存到磁盘。运行时通过 `dlopen` 加载，
按字节码哈希查找函数指针，调用 `call_with_interpreter` 执行。

**AOT 加速来源**：
- 消除解释器 dispatch 循环（opcode fetch → decode → dispatch 的逐指令开销）
- LLVM 优化：常量折叠、死代码消除、寄存器分配、指令调度
- 分支预测更友好：编译后的控制流比解释器的 switch-case 更容易被 CPU 预测

**AOT 的额外开销**：
- 每帧需要计算字节码哈希 + HashMap 查找
- 编译后的本机代码体积大，占用指令缓存

### 2.2 实验环境

| 项目 | 值 |
|------|-----|
| CPU | Intel Xeon Platinum 8488C (Sapphire Rapids) |
| L1i 缓存 | 32KB/core, 8-way, 64 sets |
| L1d 缓存 | 48KB/core, 12-way |
| L2 缓存 | 2MB/core |
| iTLB (L1) | ~128 条目 (4KB 页), 8-way |
| STLB (L2) | 2048 条目 |
| 页大小 | 4KB |
| 目标链 | OP Stack (Isthmus, EthSpec=Prague) |
| 测试区块 | #38004930 (221 笔非 deposit 交易, 304 个唯一合约) |

## 3. 问题发现：All-AOT 为何只有 1.17x？

### 3.1 初始预期与现实

EVM 解释器的 dispatch 循环（每条指令一次 switch/match）是已知的性能瓶颈。理论上 AOT 编译
应该带来显著加速。但在真实区块上：

- exec_loop 占 handler.run() 的 99.1%，非执行开销仅 0.9%
- 即便没有 Amdahl 定律的稀释，全链路加速也只有 1.17x

### 3.2 第一层原因：真实交易是存储密集型的

真实 EVM 交易大量使用 SLOAD/SSTORE/CALL。这些操作的开销在数据库访问（CacheDB 的 HashMap
查找）和帧创建/销毁，AOT 对此无能为力。AOT 只加速纯计算指令（ADD、MUL、SHR 等）和栈操作。

但这不是全部原因。

### 3.3 第二层原因（关键发现）：指令缓存压力

通过 `perf stat` 对整个区块进行硬件计数器采样（30 轮），我们发现了 AOT 模式的隐藏成本：

| 计数器 | All-AOT | Native | AOT/Native | MPKI 比率 |
|--------|---------|--------|-----------|----------|
| L1-icache-miss | 122.5M | 38.4M | 3.19x | **3.84x** |
| iTLB-miss | 1.18M | 56K | 20.9x | **25.2x** |
| branch-miss | 10.3M | 36.0M | 0.29x | 0.35x (AOT 更好) |
| dTLB-miss | 215K | 141K | 1.53x | 1.83x (次要) |
| IPC | 2.91 | 2.64 | 1.10x | — |

> MPKI = Misses Per Kilo Instructions，归一化后更能反映单位工作量的缓存效率。

**关键发现**：

1. **iTLB 是最极端的瓶颈**：AOT 的 iTLB miss MPKI 是 Native 的 **25 倍**。304 个 AOT
   函数总计 49MB `.text` 段，映射到 12,775 个 4KB 页——而 L1 iTLB 只有 ~128 个条目。
   每次执行不同合约的 AOT 函数，几乎必然触发 iTLB miss。

2. **L1i 贡献了最大的绝对周期开销**：84M 次额外 miss × ~10 周期/miss ≈ 占 AOT 总时间
   的 24%。32KB 的 L1i 缓存只能容纳约 4-8 个 AOT 函数（每个 2-8KB），304 个函数远超容量。

3. **分支预测是 AOT 的优势**：AOT 代码的分支 miss 率只有 Native 的 1/3。编译后的直接
   跳转比解释器的间接分支（switch-case）更易预测，这部分补偿了缓存惩罚。

4. **AOT 仍然胜出**：虽然缓存压力巨大，但 AOT 的指令数少 17%、IPC 高 10%、分支预测好
   2.9x，综合起来仍然净赚 1.17x。

### 3.4 合并 .so 无效：瓶颈是代码总量

一个自然的假设是：304 个独立的 `.so` 文件通过 `dlopen` 分散在地址空间中，导致 iTLB 碎片化。

我们通过实验验证了这一假设——**结论是它不成立**：

| 模式 | AOT 函数数 | 执行时间 | vs Native |
|------|-----------|---------|-----------|
| Native | 0 | 21.35ms | 1.00x |
| Separate .so (默认) | 304 | 16.47ms | 1.30x |
| Merged .so (合并) | 304 | 16.64ms | 1.28x |

将 304 个 `.o` 链接为一个 `merged_all.so`（52MB），性能与分散的 `.so` 没有统计显著差异。

**原因**：瓶颈不是地址空间碎片，而是代码总量。即使连续存放，49MB 代码 = 12,775 页 >> iTLB
128 条目，溢出比达 100 倍。合并 `.so` 只解决了碎片问题，但总页数不变。

## 4. 实验方法：Fullblock Ablation

### 4.1 为什么需要新方法

此前的 `ablation_bench` 在**隔离环境**（单合约独立快照）中测量每个合约的 AOT 效果。但这
忽略了 L1i/iTLB 压力——隔离环境中几乎没有其他 AOT 函数竞争缓存。

以 tx185 涉及的一个合约为例：

| 环境 | AOT 耗时 | Native 耗时 | AOT 效果 |
|------|---------|------------|---------|
| 隔离 (isolated) | 55.0µs | 93.7µs | 1.70x 加速 |
| 前缀 (prefix 0→185) | 159.6µs | 109.5µs | 0.69x 减速 |
| 全区块 (fullblock) | 136.2µs | 119.9µs | 0.88x 减速 |

同一个合约在隔离环境下是 1.70x 加速，但在全区块执行中变成 0.88x 减速！原因是前序交易的
AOT 函数已经把 L1i/iTLB 填满，这个合约的 AOT 代码被加载时触发了大量缓存 miss。

### 4.2 Fullblock Ablation 方法

为获得真实条件下的 AOT 效果评估，我们设计了 `fullblock_ablation`：

**Phase 1 — Baseline**：执行完整区块 R 轮，分别测量 All-AOT 和 Native 的总时间。

**Phase 2 — Discovery**：使用 `DiscoveryHandler`（自定义 Handler 实现）记录每笔交易
触及的合约字节码哈希，建立交易→合约映射。

**Phase 3 — Ablation**：对每个唯一合约 C：
  1. 从 All-AOT 的函数映射中移除 C（其他 303 个合约仍用 AOT）
  2. 执行完整区块 R 轮，记录总时间
  3. 恢复 C，用 Welch's t-test 比较 "有 C" vs "无 C"
  4. 若移除 C 后显著变快 → C 是 BAD（AOT 有害）
  5. 若移除 C 后显著变慢 → C 是 GOOD（AOT 有益）
  6. 无显著差异 → NEUTRAL

**统计检验**：Welch's t-test，显著性水平 α=0.05。零依赖实现（Lanczos gamma +
Lentz 连分数），约 100 行 Rust 代码。

### 4.3 隔离 vs 全区块消融的差异

| 实验 | GOOD | BAD | NEUTRAL |
|------|------|-----|---------|
| 隔离消融 (ablation_bench, rounds=15) | 137 | 19 | 89 |
| **全区块消融 (fullblock_ablation, rounds=15)** | **34** | **127** | **84** |

在真实缓存压力下，103 个合约从 GOOD 翻转为 BAD/NEUTRAL。这证明了隔离测量严重高估了
AOT 的收益。

## 5. 实验结果

### 5.1 Fullblock Ablation 分布

在 block 38004930 上（245 个通过 discovery 确认被使用的合约，rounds=15, warmup=3）：

- **GOOD（AOT 有益）**：34 个合约（14%）
- **BAD（AOT 有害）**：127 个合约（52%）
- **NEUTRAL（无显著差异）**：84 个合约（34%）

> 超过一半的合约被 AOT 后反而拖慢了整体执行！

### 5.2 四种策略对比

基于 ablation 结果，我们构建了四种 AOT 策略并在全区块上对比（rounds=15）：

| 策略 | AOT 函数数 | 中位数时间 | vs Native | 说明 |
|------|-----------|----------|-----------|------|
| Native | 0 | 21.10ms | 1.00x | 纯解释器 |
| Good-Only | 34 | 18.94ms | 1.11x | 只编译 GOOD |
| All-AOT | 304 | 18.07ms | 1.17x | 编译所有合约 |
| **No-Bad** | **177** | **17.39ms** | **1.21x** | 移除 BAD，保留 GOOD+NEUTRAL |

**关键发现**：

1. **No-Bad 是最优策略**：移除 127 个 BAD 合约后，减少了 L1i/iTLB 压力，使剩余 177 个
   合约的 AOT 效果更好。1.21x 显著优于 All-AOT 的 1.17x。

2. **Good-Only 过于激进**：只保留 34 个 GOOD 合约失去了 NEUTRAL 合约的潜在收益。虽然每个
   NEUTRAL 合约单独看效果不显著，但它们的 AOT 代码仍然比解释器快（只是差值在噪声范围内），
   且数量多（84 个），累积效果可观。

3. **All-AOT 不是最优**：盲目编译所有合约引入了不必要的缓存压力。127 个 BAD 合约的 AOT
   代码占用 L1i/iTLB 但不带来净收益。

## 6. 结论与建议

### 6.1 核心结论

1. **AOT 编译的收益与缓存成本是一个权衡**。每多编译一个合约，获得该合约的 dispatch 消除
   收益，但同时增加所有合约的 L1i/iTLB 压力。

2. **Selective AOT（No-Bad 策略）优于 All-AOT**。在真实区块上，No-Bad（177 函数）
   比 All-AOT（304 函数）快 3.4%（17.39ms vs 18.07ms），比 Native 快 21%。

3. **隔离测量不可信**。必须在真实缓存压力下评估 AOT 效果。隔离消融高估 GOOD 数量 4 倍
   （137 vs 34）。

4. **合并 .so 无效**。指令缓存瓶颈是代码总量（49MB >> 32KB L1i），不是地址空间碎片。

### 6.2 后续方向

1. **多区块验证**：在更多区块上运行 fullblock_ablation，验证 No-Bad 策略的泛化性。BAD
   合约列表是否跨区块稳定？

2. **BAD 合约特征分析**：分析 BAD 合约的字节码大小、帧调用次数、opcode 分布，找到无需
   ablation 就能预测 BAD 的启发式规则。

3. **大页（Huge Pages）**：使用 2MB 大页（透明或显式）加载 AOT 代码，将 iTLB 需求从
   12,775 条降至 ~25 条，可能显著缓解 iTLB 瓶颈。

4. **分层编译（Tiered Compilation）**：运行时统计合约调用频次，仅对高频合约启用 AOT，
   低频合约保持解释执行。

---

## 附录 A: 实验工具与复现命令

### 工具一览

| 工具 | 路径 | 用途 |
|------|------|------|
| `fullblock_ablation` | `examples/runner/src/bin/fullblock_ablation.rs` | 全区块消融，生成 whitelist/blacklist |
| `icache_experiment` | `examples/runner/src/bin/icache_experiment.rs` | 6 种模式的 perf stat 友好工具 |
| `ablation_bench` | `examples/runner/src/bin/ablation_bench.rs` | 隔离消融（单合约独立快照） |
| `frame_bench` | `examples/runner/src/bin/frame_bench.rs` | 逐帧 AOT vs Native 计时 |
| `bin_bench` | `examples/runner/src/bin/bin_bench.rs` | 多区块 AOT benchmark |

### 复现命令

```bash
# 1. 预编译 AOT 缓存
cargo run -p revmc-examples-runner --bin precompile --release -- \
  --dir /path/to/bench_data --start 38004930 --count 1 --cache-dir /tmp/jit_cache

# 2. 全区块消融（核心实验）
cargo run -p revmc-examples-runner --bin fullblock_ablation --release -- \
  --block 38004930 --cache-dir /tmp/jit_cache --rounds 15 --warmup 3

# 3. perf stat 硬件计数器采集
perf stat -e L1-icache-load-misses,iTLB-load-misses,branch-misses,dTLB-load-misses,instructions \
  ./target/release/icache_experiment --block 38004930 --tx-index 185 --rounds 30 \
  --mode fullblock_jit

# 4. 选择性 AOT benchmark（使用 fullblock_ablation 输出的 whitelist）
cargo run -p revmc-examples-runner --bin fullblock_ablation --release -- \
  --block 38004930 --cache-dir /tmp/jit_cache --benchmark
```

### 输出文件

- `/tmp/fullblock_whitelist.json`：GOOD/BAD 合约哈希列表
- 标准输出：逐合约分析报告 + 整体统计

## 附录 B: 辅助实验

### B.1 Frame Bench（逐帧分析）

使用 `frame_bench` 对每笔交易的每个 EVM 帧分别计时：

- All-JIT 是最优策略（优于任何选择性阈值）
- 按调用次数筛选：阈值越高，性能越差
- 冷启动 whitelist（无 warmup）：仅 22/245 合约，0.91x（比 Native 更差）

**关键教训**：`Instant::now()` 的逐帧计时开销对 AOT 影响更大（AOT 帧执行更快 → 计时
开销占比更高）。逐帧测量会系统性地低估 AOT 收益。

### B.2 隔离消融（Ablation Bench）

在独立快照环境中，逐合约移除并用 Welch's t-test 检验：

| rounds | GOOD | BAD | NEUTRAL |
|--------|------|-----|---------|
| 3 (noisy) | 93 | 53 | 99 |
| 15 (stable) | 137 | 19 | 89 |

更多轮次 → 更高统计功效 → 更多 NEUTRAL 被分类为 GOOD（真实效果虽小但统计显著）。

但如第 4 节所述，隔离环境严重高估了 AOT 的收益。

### B.3 Icache Experiment（缓存压力实验）

6 种模式（isolated/prefix/fullblock × jit/native），设计用于配合 `perf stat` 使用：

- **isolated**：只执行目标交易（无缓存压力）
- **prefix**：执行区块前缀到目标交易（渐进缓存压力）
- **fullblock**：执行完整区块（真实缓存压力）

第 3.3 节的 perf stat 数据来自此工具。
