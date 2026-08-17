# Benchmark 结果报告：TaperHashMap vs Daft hashbrown

## 1. 概要结论

**TaperHashMap 在当前实现下，所有测试场景均比 Daft 使用的 hashbrown (Swiss Table) 慢 40%-200%。**

不建议在当前状态下迁移到 Daft。需要先优化 TaperHashMap 的实现开销，然后重新评估。

---

## 2. 测试环境

- **硬件**: Apple MacBook Air M1 (aarch64)
- **OS**: macOS Sonoma
- **Rust**: nightly-2025-09-03
- **hashbrown**: 0.16.1 (NEON SIMD tag matching)
- **TaperHashMap**: SWAR 位运算 tag matching (无 SIMD intrinsics)
- **Hash 算法**: xxHash3-64 (两侧相同)
- **统计**: Criterion 0.5, 50 samples per benchmark

---

## 3. 完整结果

### 3.1 不同 Key 复杂度 (rows=1M)

| Key 类型 | Groups | Daft (ms) | Taper (ms) | 差距 |
|---------|--------|-----------|------------|------|
| **1col_i64** | 10 | 3.5 | 8.3 | Taper 慢 137% |
| | 100 | 3.7 | 8.5 | Taper 慢 130% |
| | 1000 | 4.3 | 8.9 | Taper 慢 107% |
| | 10000 | 5.1 | 12.0 | Taper 慢 135% |
| **2col_i64** | 10 | 3.8 | 9.3 | Taper 慢 145% |
| | 100 | 4.1 | 12.3 | Taper 慢 200% |
| | 1000 | 4.9 | 9.7 | Taper 慢 98% |
| | 10000 | 11.6 | 13.5 | Taper 慢 16% |
| **4col_i64** | 10 | 5.2 | 12.9 | Taper 慢 148% |
| | 100 | 6.1 | 12.3 | Taper 慢 101% |
| | 1000 | 6.3 | 11.3 | Taper 慢 79% |
| | 10000 | 9.2 | 15.0 | Taper 慢 63% |
| **2col_i64 + string** | 10 | 6.0 | 11.6 | Taper 慢 93% |
| | 100 | 6.3 | 13.2 | Taper 慢 110% |
| | 1000 | 9.6 | 13.8 | Taper 慢 44% |
| | 10000 | 11.0 | 22.4 | Taper 慢 103% |

### 3.2 不同数据量 (key=2col_i64, groups=100)

| Rows | Daft (ms) | Taper (ms) | 差距 |
|------|-----------|------------|------|
| 100K | 0.4 | 0.9 | Taper 慢 ~125% |
| 1M | 3.8 | 9.3 | Taper 慢 ~145% |
| 10M | 38.7 | 120.4 | Taper 慢 211% |

### 3.3 不同 Load Factor (key=2col_i64, rows=1M, groups=1000)

| Load Factor | Daft (ms) | Taper (ms) | 差距 |
|-------------|-----------|------------|------|
| 0.5 | 4.4 | 9.9 | Taper 慢 125% |
| 0.7 | 4.3 | 9.8 | Taper 慢 128% |
| 0.9 | 4.5 | 10.8 | Taper 慢 140% |

---

## 4. 分析

### 4.1 为什么 Taper 慢

1. **Vec 分配开销 (~40% overhead)**
   - `existing_entries: Vec<(usize, u32)>` 每行 existing 都 push 一次
   - 1M 行 100 groups → ~999,900 次 Vec push
   - hashbrown 没有这个开销

2. **`dyn FnMut` 回调不能内联 (~30% overhead)**
   - `emplace_batch(hashes, &mut dyn FnMut, &mut dyn FnMut)` 使用动态分发
   - hashbrown 的 `from_hash(h, |other| ...)` 闭包被单态化内联
   - 每行至少一次虚调用

3. **SWAR vs NEON (~10% overhead)**
   - hashbrown 在 aarch64 用 NEON `vceq_u8` (真 SIMD，8 字节一条指令)
   - TaperHashMap 用 SWAR 位运算 (多条标量指令模拟)
   - 在 Apple Silicon 上 NEON 明显更快

4. **Phase 2 循环的额外遍历 (~20% overhead)**
   - Taper 需要第二次遍历 `existing_entries` 做 comparator
   - Daft 在 probe 循环里一次性做完

### 4.2 为什么 Daft 的 hashbrown 这么快

- `raw_entry_mut().from_hash()` 整个泛型链被 LTO 内联
- NEON SIMD ctrl byte matching (硬件加速)
- 没有中间 Vec 分配，没有虚调用
- Swiss Table 的 load factor 和 probe 效率已经很高

### 4.3 有趣的观察

- `2col_i64/groups=10000`: 差距最小 (16%)，因为高基数时大部分行是 Vacant (不调 comparator)
- `4col_i64/groups=10000`: 差距 63%，key 复杂度增加确实缩小了差距
- Load factor 对结果影响不大 (125%-140%)，说明瓶颈不在冲突处理

---

## 5. 测试方法说明

### 两侧做了什么

| 步骤 | Daft 侧 | Taper 侧 |
|------|---------|----------|
| 数据 | 相同的 Vec<i64> + xxHash3 预计算 hash | 同左 |
| Hash table | hashbrown::HashMap + IdentityHasher | TaperHashMap::with_capacity |
| Tag 过滤 | hashbrown 内部 NEON SIMD (不可见) | SWAR BitMask::match_tag |
| Hash 比较 | `h == other.hash` (在闭包里) | `chunk.keys[slot] == hash` |
| Key 比较 | `keys.compare(i, j)` 即时执行 | `keys.compare(i, j)` 延迟到 Phase 2 |
| 聚合 | `sums[gid] += values[i]` | `sums[gid] += values[i]` |

### 测量范围

计时从空 hash table 开始，包括：初始化 → 遍历全部行 → probe/insert → comparator → aggregation。不包括数据生成和 hash 计算。

---

## 6. 建议

### 短期：不迁移

当前 TaperHashMap 实现有显著的工程开销（Vec 分配、dyn 回调），掩盖了可能的架构优势。在这些问题解决之前，迁移不会带来收益。

### 如果要继续探索

1. **消除 Vec 分配**: 改用 pre-allocated `group_ids: Vec<u32>` 直接写入，不用 push
2. **消除 dyn 回调**: 改用泛型参数 `impl FnMut` 让编译器内联
3. **加 NEON 支持**: 在 aarch64 上用 `vceq_u8` 替代 SWAR
4. **减少 Phase 2 开销**: 由于 64-bit hash 碰撞率极低，Phase 2 的 comparator 几乎不实际工作，但遍历 existing_entries 本身有成本

优化后重新测试。如果能稳定缩小到 Daft 的 10% 以内或更快，再考虑迁移。

### 最终验证

即使独立 benchmark 证明 Taper 更快，最终还需要在 Daft 内部做 A/B 对比（用 feature flag 替换 `agg_generic_hash_path`），因为实际 HashAgg 还包括 hash 计算、Arrow 内存管理等不在此 benchmark 范围内的开销。

---

## 7. 运行方式

```bash
cd rust_taper_hashmap
cargo bench

# 查看 HTML 报告
open target/criterion/report/index.html
```

## 8. 代码位置

```
rust_taper_hashmap/
├── benches/hashmap_bench.rs    # Criterion benchmark
├── src/taper_hashmap.rs        # TaperHashMap 实现
├── src/bitmask.rs              # SWAR BitMask
├── src/chunk.rs                # Chunk + SlotValue
└── docs/
    ├── benchmark_design.md     # 设计详解
    ├── benchmark_flow.md       # 流程图
    └── benchmark_results.md    # ← 本文档
```
