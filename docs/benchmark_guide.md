# TaperHashMap vs hashbrown Benchmark 说明

## 目标

验证 TaperHashMap 是否值得迁移到 Daft，回答：

> 在 Daft HashAgg 的典型工作负载下，TaperHashMap 比 hashbrown (Swiss Table) 快多少？优势从哪开始？

---

## 运行方法

```bash
cd rust_taper_hashmap

# 跑全部 benchmark
cargo bench

# 只跑某一组
cargo bench --bench hashmap_bench -- "key_1col_i64"
cargo bench --bench hashmap_bench -- "row_scale"
cargo bench --bench hashmap_bench -- "load_factor"

# 快速验证 (缩短测量时间)
cargo bench --bench hashmap_bench -- --warm-up-time 1 --measurement-time 3 "key_2col_i64"
```

结果输出在终端，HTML 报告在 `target/criterion/report/index.html`。

---

## 三组 Benchmark

### 1. `bench_key_complexity` — 不同 key 复杂度

| 参数 | 值 |
|------|---|
| rows | 1,000,000 (固定) |
| groups | 10, 100, 1000, 10000 |
| key 类型 | `1col_i64`, `2col_i64`, `4col_i64`, `2col_i64_string` |
| load factor | ~0.9 (自动) |

**目的**：找到 Taper 优势的拐点。key 越复杂 (列越多 / 有 string)，Daft 的 comparator 越贵，Taper 的 deferred compare 优势越大。

**预期**：
- `1col_i64`: Daft 可能更快 (comparator 只是一个 i64 ==，几乎免费)
- `4col_i64`: 两者接近
- `2col_i64_string`: Taper 应该有优势 (string compare 贵)

---

### 2. `bench_row_scale` — 不同数据量

| 参数 | 值 |
|------|---|
| rows | 100,000 / 1,000,000 / 10,000,000 |
| groups | 100 (固定) |
| key 类型 | `2col_i64` (固定) |
| load factor | ~0.9 (自动) |

**目的**：验证优势是否随数据量线性放大。如果 Taper 在 1M 行快 5%，在 10M 行是否也快 5%？

**预期**：比例应该稳定，因为 hash table 建好后 probe 是 O(1)。绝对时间线性增长但比例不变。

---

### 3. `bench_load_factor` — 不同装载率

| 参数 | 值 |
|------|---|
| rows | 1,000,000 (固定) |
| groups | 1,000 (固定) |
| key 类型 | `2col_i64` (固定) |
| initial capacity | groups/0.5, groups/0.7, groups/0.9 |

**目的**：高 load factor 意味着更多碰撞、更多 probe 步进。Taper 的 chunk 线性步进和 SWAR tag 是否在高碰撞下有优势？

**预期**：
- lf=0.5: 几乎无碰撞，两者差距小
- lf=0.9: 碰撞增多，Taper 的 tag 预过滤优势可能显现

---

## 两侧做了什么（公平性）

### Daft 侧 (hashbrown)

```
for each row:
  1. raw_entry_mut().from_hash(h, closure)
     → hashbrown 内部: NEON ctrl byte match (SIMD)
     → closure: hash== + comparator(逐列比较)
  2. Occupied → sums[gid] += values[i]
     Vacant → insert + sums.push(0)
```

### Taper 侧

```
Phase 1: emplace_batch(hashes)
  for each row:
    1. SWAR tag match (纯位运算)
    2. hash== (int64)
    3. New → write gid to SlotValue
       Existing → read gid from SlotValue, push to existing_entries

Phase 2: deferred compare
  for each existing_entry:
    1. comparator(idx, rep_row) — 和 Daft 相同的逐列比较
    2. sums[gid] += values[idx]
```

**关键区别**：
- Daft: 每行命中时**立即**调 comparator
- Taper: 命中行**攒起来**，batch 结束后统一 compare

**相同点**：
- 相同 hash 值（固定 seed）
- 相同 comparator 逻辑（逐列 i64/string 比较）
- 相同 aggregation（sums[gid] += val）
- 相同初始容量

---

## 当前已知的不公平之处

| 问题 | 偏向 | 影响 |
|------|------|------|
| Taper 侧有额外 Vec 分配 (`new_entries`, `existing_entries`) | 偏向 Daft | 高，~1M push 操作 |
| Daft 的 hashbrown 用 NEON SIMD tag match | 偏向 Daft | 中，比 SWAR 快 |
| Taper 的 `dyn FnMut` 回调不能内联 | 偏向 Daft | 中，阻止编译器优化 |
| Daft 的 `raw_entry_mut` 闭包被单态化内联 | 偏向 Daft | 高 |

如果 Taper 即使在这些不利条件下仍然更快，说明架构优势足够大。如果不快，需要先优化这些 overhead 再下结论。

---

## 判断标准

| 结果 | 决策 |
|------|------|
| Taper 全场景慢 10%+ | ❌ 不迁移，架构优势不足以覆盖 overhead |
| Taper 在 string key / 高 cardinality 下快 5-10% | ⚠️ 有潜力，先优化实现再测 |
| Taper 在多数场景稳定快 10%+ | ✅ 值得做 Daft minimal integration |
| Taper 在特定场景快 20%+ | ✅ 立即迁移该场景 |

---

## 下一步

1. 跑完全部 benchmark，整理结果表
2. 如果 Taper 慢 → 分析瓶颈，优化实现，重测
3. 如果 Taper 快 → 做 Daft minimal integration (feature flag)
4. 最终用 TPC-H Q1/Q18 做端到端验证

---

## 文件结构

```
rust_taper_hashmap/
├── Cargo.toml                     # 依赖: criterion, hashbrown, rand
├── benches/
│   └── hashmap_bench.rs           # Criterion benchmark (本文档描述的)
├── src/
│   ├── lib.rs
│   ├── taper_hashmap.rs           # TaperHashMap 实现
│   ├── chunk.rs                   # Chunk + SlotValue
│   ├── bitmask.rs                 # BitMask SWAR
│   ├── row_container.rs           # RowContainer (未完全集成到 benchmark)
│   ├── batch_compare.rs           # BatchComparer (未完全集成)
│   └── hash.rs                    # Hash utilities
├── docs/
│   ├── DESIGN.md                  # 整体设计
│   ├── call_chain_and_dataflow.md # 调用链
│   └── benchmark_guide.md         # ← 本文档
└── target/
    └── criterion/                 # benchmark 结果和 HTML 报告
```
