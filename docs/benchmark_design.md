# TaperHashMap vs Daft (hashbrown) Benchmark 设计文档

## 1. 目标

在纯 Hash Table 层面对比 **OmniOperator TaperHashTable**（Rust 实现）与 **Daft 风格 hashbrown**（业界标准 Robin Hood hash table）在 GROUP BY 聚合场景下的性能表现。

重点验证：
- TaperHashTable 的 batch prefetch + SIMD 批量比较策略的实际收益
- VARCHAR（变长字符串）key 对性能的影响
- 不同 Hash Table 规模和数据分布下的表现差异

---

## 2. 测试架构

### 2.1 Taper 侧

使用完整的 C++ TaperHashTable 5-step 流水线的 Rust 等价实现：

```
Step 1: Hash 计算（预计算，不计入 benchmark 时间）
Step 2: emplace_batch_full — tag + hash(u64) 两层过滤 + 软件 prefetch
Step 3: StoreKeyOneRowFromDecode — 序列化 key 到 RowContainer
Step 4: GetUnequalsNumWithDecode — SIMD 批量 key 验证（indices-swap）
Step 5: Collision repair — 单行 emplace + 完整 key 比较
```

核心组件：
| 组件 | 对应 C++ | 功能 |
|------|---------|------|
| `TaperHashMap` | `TaperFlatHashTable` | chunked open-addressing, 128B aligned |
| `RowContainer` | `RowContainer` | 行式存储 + arena allocator |
| `TaperColumnSerializeHandler` | `TaperColumnSerializeHandler` | 5-step 流水线编排 |
| `batch_compare_decoded_i64_neon` | `SveBatchCompareDecoded<int64>` | NEON SIMD 2×i64 并行比较 |
| `compare_varchar_from_row` | `CompareVarcharFromRow` | NEON 16B/iter 字节比较 |

### 2.2 Daft 侧

使用 hashbrown `HashMap` + `raw_entry_mut` API，模拟 Daft query engine 的 GROUP BY 实现：

```
for each row:
    raw_entry_mut.from_hash(hash, |existing| {
        compare all key columns: existing[col] == input[col]
    })
    → Occupied: accumulate agg
    → Vacant: insert new group
```

特点：
- 逐行处理（无 batch）
- 列式直接访问输入数据（无序列化）
- Robin Hood 开放寻址 + SIMD 查找

---

## 3. 测试参数

### 3.1 Key 类型组合

| 名称 | VARCHAR 列数 | INT64 列数 | 总列数 |
|------|------------|-----------|--------|
| `0str_4int` | 0 | 4 | 4 |
| `1str_3int` | 1 | 3 | 4 |
| `2str_2int` | 2 | 2 | 4 |
| `3str_1int` | 3 | 1 | 4 |
| `4str_0int` | 4 | 0 | 4 |

### 3.2 Hash Table 参数

| 参数 | 取值 | 含义 |
|------|------|------|
| HT Size (slots) | 16,384 / 65,536 / 262,144 / 1,048,576 | Hash table 容量 |
| Load Factor | 0.50 / 0.75 | `num_groups = HT_size × LF` |
| Selectivity | 0.1 / 0.3 / 0.5 / 0.7 / 0.9 | probe 命中率 |

### 3.3 固定参数

| 参数 | 值 |
|------|---|
| Probe 行数 | 1,000,000 |
| 字符串长度 | ~10-12 bytes (ASCII) |
| 聚合操作 | SUM (i64 += i64) |
| Hash 函数 | xxh3_64 |
| Sample size | 20 (Criterion) |

---

## 4. 数据生成

```
Build phase:
  生成 num_groups 个 distinct key 组合
  str_col[c][i] = "key_{i}_c{c}" (确定性)
  int_col[c][i] = i * (97 + c*31) + 1 (确定性)

Probe phase:
  hits = num_probe_rows × selectivity (从 build keys 随机选)
  misses = num_probe_rows × (1-selectivity) (保证不在 build set)
  shuffle(hits + misses) → 模拟真实交错访问

Final data = [build keys] + [shuffled probe keys]
```

---

## 5. 计时范围

两侧计时范围一致：**从创建 hash table 到处理完全部 1M 行**。

| 步骤 | Taper | Daft | 计入时间? |
|------|-------|------|----------|
| Hash 计算 | 预计算 | 预计算 | ❌ |
| 数据生成 | 预生成 | 预生成 | ❌ |
| 创建 HT | `TaperColumnSerializeHandler::new` | `HashMap::with_capacity` | ✅ |
| 处理所有行 | `emplace_table_with_decode` | for loop + raw_entry_mut | ✅ |

---

## 6. SIMD 加速路径

### 6.1 INT64 列比较 (aarch64 NEON)

```rust
// batch_compare_decoded_i64_neon: 2×i64 并行比较
let v_stored = vcombine_s64(vcreate_s64(stored0), vcreate_s64(stored1));
let v_input = vcombine_s64(vcreate_s64(input0), vcreate_s64(input1));
let cmp = vceqq_s64(v_stored, v_input);  // 128-bit 并行
```

### 6.2 VARCHAR 字节比较 (aarch64 NEON)

```rust
// compare_bytes_neon: 16 bytes/iteration
let lhs_vec = vld1q_u8(left.add(i));
let rhs_vec = vld1q_u8(right.add(i));
let cmp = vceqq_u8(lhs_vec, rhs_vec);  // 16 字节并行比较
```

### 6.3 Prefetch

```rust
// 每个 chunk 128B = 2 cache lines, 提前 16 步 prefetch
asm!("prfm pldl1keep, [{ptr}]");      // cache line 1
asm!("prfm pldl1keep, [{ptr + 64}]"); // cache line 2
```

---

## 7. VARCHAR Key 序列化

### 7.1 存储模型

```
Row 布局: [int_col(8B)][varchar_ptr(8B)]...[null_block][agg_state]
                              │
                              ▼ Arena Buffer
                        [rowLenSize:1B][length:1/2/4B][data:N bytes]
```

### 7.2 多 VARCHAR 列合并 (Merged)

当 varchar 列 > 1 时，所有 varchar 数据合并到一个连续 block，只存一个指针：

```
Row: [ptr(8B)][unused]...[int cols][null][agg]
       │
       ▼ 一个连续 arena block
       [col0: rls+len+data][col1: rls+len+data][col2: rls+len+data]
```

比较时先用 `GetAllMergedVarcharPtrs` 缓存各列指针，再逐列 `CompareVarcharFromRow`。

---

## 8. 测试结果摘要

### 8.1 关键发现

| Key 类型 | Taper vs Daft | 原因分析 |
|---------|--------------|---------|
| **0str_4int** | **Taper 赢 30-50%** | SIMD batch 比较 + prefetch 发挥优势 |
| **1str_3int** | **接近平手~Taper 微赢** | 1 个 varchar 开销小 |
| **2str_2int** | **Daft 赢 10-30%** | varchar 序列化开销开始影响 |
| **3str_1int** | **Daft 赢 30-60%** | varchar 主导，arena 开销大 |
| **4str_0int** | **Daft 赢 50-200%** | 全 varchar，序列化 + pointer chase |

### 8.2 Selectivity 影响

- Selectivity 高 (0.7-0.9) → Taper 相对更优（batch compare 发挥作用）
- Selectivity 低 (0.1-0.3) → Taper 相对更差（大量新 group 创建 = 序列化开销大）

### 8.3 HT Size 影响

- 中等 HT (65K-262K) → Taper prefetch 优势明显
- 大 HT (1M) → 两侧都受 cache miss 影响，差距缩小
- 小 HT (16K) → 全在 cache 中，prefetch 无用

### 8.4 代表性数据点

| Case | Daft | Taper | 比值 |
|------|------|-------|------|
| 0str_4int, ht=65K, sel=0.9 | 52ms | **26ms** | **0.50x** ✅ |
| 0str_4int, ht=262K, sel=0.1 | 62ms | **35ms** | **0.57x** ✅ |
| 1str_3int, ht=16K, sel=0.9 | 48ms | **40ms** | **0.85x** ✅ |
| 2str_2int, ht=65K, sel=0.7 | 88ms | 91ms | 1.03x ≈平 |
| 4str_0int, ht=16K, sel=0.9 | 80ms | 113ms | 1.41x ❌ |
| 4str_0int, ht=1M, sel=0.1 | 82ms | 373ms | 4.55x ❌ |

---

## 9. 结论

1. **TaperHashTable 在 INT-heavy 场景下确实优于 hashbrown**，验证了 batch prefetch + SIMD 策略的有效性。

2. **VARCHAR 场景下 Taper 的行式序列化设计是性能瓶颈**——arena 分配 + 格式编解码 + pointer chase 的代价超过了 batch/prefetch 的收益。

3. **Taper 的优势随 HT 规模和 selectivity 增大而增强**——这符合其 batch prefetch 隐藏 cache miss latency 的设计初衷。

4. **实际 query engine 中 Taper 的 buffer 复用（本 benchmark 未模拟）会进一步缩小差距**——C++ Taper 的 `BatchContext` 等 buffer 是类成员常驻复用的。

---

## 10. 运行方式

```bash
# 全部运行
cargo bench

# 只跑纯 int
cargo bench -- "0str_4int"

# 只跑特定参数
cargo bench -- "2str_2int_ht=65536_lf=0.50_sel=0.7"

# 验证 SIMD 路径
cargo test test_simd_dispatch_i64 -- --nocapture
```

---

## 11. 文件结构

```
src/
├── taper_hashmap.rs       ← TaperFlatHashTable (chunked HT + prefetch)
├── chunk.rs               ← 128B aligned chunk + SlotValue
├── bitmask.rs             ← SWAR tag matching
├── row_container.rs       ← RowContainer (行存储 + arena)
├── batch_compare.rs       ← SIMD batch key compare (NEON)
├── column_marshaller.rs   ← TaperColumnSerializeHandler (5-step pipeline)
└── lib.rs

benches/
└── hashmap_bench.rs       ← Criterion benchmark (Taper vs Daft)

docs/
└── BENCHMARK_DESIGN.md    ← 本文档
```
