# TaperHashMap vs Daft (hashbrown) Benchmark 设计文档

## 1. 目标

对比 TaperHashMap（chunked open-addressing + SWAR + software prefetch）和 Daft 现有方案（hashbrown Swiss Table + Arrow 列式比较）在 hash aggregation 场景下的吞吐性能。

验证 Taper 的设计假设：通过 software prefetch + collision batching 隐藏内存延迟，在大 group 数场景下优于 hashbrown 的逐行随机访问模式。

---

## 2. 测试场景

模拟 `SELECT agg(val) FROM table GROUP BY key_col1 [, key_col2, ...]` 的核心热路径：hash table 的 emplace（查找或插入）操作。

不包含：I/O、表达式求值、调度框架开销。纯 hash table 层面的微基准测试。

---

## 3. 参数矩阵

| 参数 | 取值 | 含义 |
|------|------|------|
| Key Type | `1col_i32`(4B), `1col_i64`(8B), `2col_i64`(16B), `4col_i64`(32B) | group-by key 的列数和宽度 |
| HT Size | 256, 1024, 4096, 16384 | hash table 初始容量参数 |
| Load Factor | 0.5, 0.75 | unique group 数 = ht_size × load_factor |
| Selectivity | 0.1 ~ 0.9 (步长 0.1) | probe 阶段命中已有 group 的比例 |

总组合数：4 × 4 × 2 × 9 = **288 个测试点**，每个点跑 Taper 和 Daft 两个实现。

---

## 4. 数据生成策略

### 4.1 整体结构

```
BenchData.hashes / keys / values:
┌──────────────────────┬─────────────────────────────────────┐
│  Build 阶段          │  Probe 阶段                         │
│  (num_keys 行)       │  (num_probe_rows = 1,000,000 行)    │
│  确定性唯一 key      │  hits + misses, 打乱顺序           │
└──────────────────────┴─────────────────────────────────────┘
```

两个 runner 处理的是同一份拼接后的数据，保证输入完全一致。

### 4.2 Build Keys

```rust
val[c][i] = i * (97 + c * 31) + 1 + c
```

- 确定性生成（无随机性），可复现
- 每列用不同线性公式，保证多列组合唯一
- 行数 = `num_keys = ht_size × load_factor`

### 4.3 Probe Keys

- **Hits**（`num_probe_rows × selectivity` 行）：从 build keys 中随机选取，保证查表一定命中
- **Misses**（剩余行）：用保证不重叠的公式生成全新 key，查表一定 miss
- 生成后做 **Fisher-Yates shuffle**，打乱 hit/miss 分布顺序

### 4.4 Hash 计算

- 单列 i32：`xxh3_64(val.to_le_bytes(), seed=0)`
- 单列 i64：`xxh3_64(val.to_le_bytes(), seed=0)`
- 多列：链式 `xxh3_64(val.to_le_bytes(), seed=prev_hash)`

两个 runner 使用完全相同的预计算 hash 数组，不在计时区间内重复计算 hash。

---

## 5. 被测实现

### 5.1 Taper 侧 (`run_taper`)

```
TaperHashMap::with_capacity(ht_size)     ← ht_size 个 chunk = ht_size×8 slot
RowContainer::new(key_sizes, agg_size=8)

调用 emplace_batch_full(hashes, key_cmp, on_init, on_update):
  - key_cmp: 解压 SlotValue 中的 6B 指针 → 去 RowContainer 逐列比较
  - on_init: rc.new_row() 分配行，写入 key 列 + 初始 agg 值
  - on_update: 累加 agg 值
```

**特点**：
- Chunk 结构（128B aligned, 8 slot/chunk）
- SWAR tag matching（标量位运算，同时比较 8 个 tag）
- Software prefetch（PREFETCH_DIST=16）
- Collision batching（碰撞行收集后批量重试）
- 指针间接：hash table → 6B ptr → RowContainer 行

### 5.2 Daft 侧 (`run_daft`)

```
hashbrown::HashMap<IndexHash, u32, IdentityBuildHasher>::with_capacity_and_hasher(ht_size)

逐行调用 raw_entry_mut().from_hash(h, comparator):
  - comparator: hash 比较 + Arrow Int64Array.value(i) == .value(j) 逐列比较
  - Vacant → insert IndexHash{idx, hash} + push agg 到 sums Vec
  - Occupied → 累加 sums[group_id]
```

**特点**：
- Swiss Table 结构（16 slot/group, flat layout）
- 真 SIMD tag matching（SSE2 `_mm_cmpeq_epi8` / NEON `vceqq_u8`）
- 无显式 prefetch
- 直接访问：hash table 内存放 IndexHash → Arrow 列式数组做比较（连续内存）

---

## 6. 公平性分析

### 6.1 已控制的变量

| 变量 | 状态 |
|------|------|
| 输入数据 | ✅ 完全相同（同一份 BenchData） |
| Hash 函数 | ✅ 相同（预计算 xxh3，不在计时内） |
| 行数 | ✅ 相同（num_keys + 1M probe） |
| 聚合逻辑 | ✅ 相同（i64 累加） |
| 随机种子 | ✅ 固定 seed=42 |

### 6.2 已知不对等

| 问题 | 影响 |
|------|------|
| **容量不对等** | `with_capacity(ht_size)` 对 Taper 是 chunk 数（×8 slot），对 Daft 是 slot 数。Taper 实际容量 8 倍大，碰撞极少。**有利于 Taper**。 |
| **比较路径不同** | Taper 需要指针解引用去 RowContainer 读 key；Daft 直接在 Arrow 数组上 `array.value(i)` 顺序访问。**有利于 Daft**。 |
| **Tag matching 实现** | Taper 用 SWAR（8 slot/次）；hashbrown 用 SSE2/NEON（16 slot/次）。**有利于 Daft**。 |
| **内存分配** | Taper 每个新 group 需要 `rc.new_row()` 分配；Daft 只 push 一个 i64。**有利于 Daft**。 |

---

## 7. Criterion 配置

```rust
group.sample_size(20);        // 每个测试点采样 20 次
// Criterion 默认测量时间 5s，warm-up 3s
// 使用 black_box 防止编译器优化掉结果
```

---

## 8. 运行方式

```bash
cargo bench                          # 跑全部 288×2 个 benchmark
cargo bench -- "1col_i64"            # 只跑 1col_i64 的组合
cargo bench -- "ht=4096"             # 只跑 ht_size=4096
cargo bench -- "taper"               # 只跑 Taper 侧
```

结果输出到 `target/criterion/` 目录，包含 HTML 报告。

---

## 9. 已知局限性与改进方向

| 局限 | 改进建议 |
|------|----------|
| 容量不对等 | Taper 改用 `with_slot_capacity(ht_size)` |
| Build + Probe 混合计时 | 分离为两个独立 benchmark：build-only 和 probe-only |
| 均匀分布 | 增加 Zipf 分布模拟热点 group |
| 只测小表（fit L1/L2） | 增加 65536、262144 等大 HT size 测 L3/DRAM 场景 |
| 无 hardware counter | 配合 `perf stat` 或 criterion-perf-events 观察 IPC/cache-miss |
| 无纯 insert 场景 | 增加 selectivity=0（全 miss）测建表极限 |
| 不测并发 | 当前为单线程，可扩展为多线程分区场景 |

---

## 10. TaperHashMap 数据结构详解

### 10.1 整体拓扑

```
TaperHashMap
┌─────────────────────────────────────────────────────────────────────┐
│  chunks: Vec<Chunk>          (长度 = 2^N, power of two)             │
│  size:   usize               (当前已用 slot 总数)                    │
│  mask:   usize               (chunks.len() - 1, 用于取模)           │
│                                                                     │
│  ┌─────────┐ ┌─────────┐ ┌─────────┐       ┌─────────┐           │
│  │ Chunk 0 │ │ Chunk 1 │ │ Chunk 2 │  ...  │Chunk N-1│           │
│  └─────────┘ └─────────┘ └─────────┘       └─────────┘           │
└─────────────────────────────────────────────────────────────────────┘

寻址:  chunk_pos = hash & mask    (低位选 chunk)
探测:  collision → pos = (pos + collision_batch) & mask  (线性步进)
扩容:  size >= capacity * 0.9 时 → 2x 扩容 + batch rehash
```

### 10.2 单个 Chunk 内存布局 (128 bytes, 2 cache lines, align=128)

```
Offset  0                              64                            128
        ├── Cache Line 0 (64B) ────────┼── Cache Line 1 (64B) ────────┤

        ┌────────┬─────────────────────────────────────┬──────┬────────────────────────────────┐
        │ tags   │            keys                      │ pad  │          values                 │
        │ [8B]   │         [64B = 8×u64]                │ [8B] │      [48B = 8×6B]               │
        └────────┴─────────────────────────────────────┴──────┴────────────────────────────────┘
        │← 8B →│←─────────── 64B ──────────────────→│← 8B →│←──────── 48B ────────────────→│

Byte 0-7:    tags[0..7]     — 8 个 1-byte tag (0x80=empty, 0x00~0x7F=occupied)
Byte 8-71:   keys[0..7]     — 8 个 u64, 存完整 64-bit hash 值
Byte 72-79:  _pad           — 8 字节对齐填充
Byte 80-127: values[0..7]   — 8 个 SlotValue (6 字节压缩指针)
```

### 10.3 单个 Slot 详解

```
Slot i (i = 0..7):

  tags[i]:    1 byte     ─── tag = (hash >> 16) & 0x7F
                              0x80 表示空，0x00~0x7F 表示已占用
                              用于 SWAR 快速筛选

  keys[i]:    8 bytes    ─── 完整 64-bit hash value
                              用于第二级比较（tag 匹配后再比 full hash）

  values[i]:  6 bytes    ─── SlotValue: 压缩指针 (低 48 位)
              ┌─────────────────────────────────────┐
              │ 指向 RowContainer 中的一行           │
              │ 那一行存着真正的 key 列数据 + agg 状态│
              └─────────────────────────────────────┘
```

### 10.4 SlotValue → RowContainer 的指针间接

```
Hash Table (Chunk 内)                    RowContainer (堆内存)
┌────────────────────┐                  ┌──────────────────────────────────┐
│ tags[i] = 0x3A     │                  │  Row 0: [col0][col1]...[agg]     │
│ keys[i] = 0xABCD.. │                  │  Row 1: [col0][col1]...[agg]     │
│ values[i] ─────────────────────────→  │  Row 2: [col0][col1]...[agg]  ← │
│                    │                  │  ...                              │
└────────────────────┘                  └──────────────────────────────────┘

RowContainer 行布局 (定长):
┌─────────────────────┬─────────────────────┬─────┬──────────────┐
│ key_col_0 (4B/8B)   │ key_col_1 (4B/8B)   │ ... │ agg_state(8B)│
└─────────────────────┴─────────────────────┴─────┴──────────────┘
│← col_offsets[0] ──→│← col_offsets[1] ──→│     │← agg_offset →│
```

### 10.5 查找/插入流程 (单行 emplace)

```
输入: hash = 0x1234_5678_9ABC_DEF0

Step 1: 定位 Chunk
         pos = hash & mask = 0x...DEF0 & 0xFF = chunk[0xF0]

Step 2: 提取 Tag
         tag = (hash >> 16) & 0x7F = (0x9ABC) & 0x7F = 0x3C

Step 3: SWAR Tag 匹配 (一次 u64 操作比较 8 个 tag)
         tags_u64 = 整个 tags[0..7] 作为 u64 读取
         match_mask = SWAR_match(tags_u64, 0x3C)

         ┌─────────────────────────────────────────────┐
         │ tag[0]=0x12  tag[1]=0x3C  tag[2]=0x80 ...   │
         │              ↑ 匹配!                         │
         └─────────────────────────────────────────────┘

Step 4: Full Hash 比较 (在 taper_hashmap.rs 内部)
         chunk.keys[slot] == hash ?  (64-bit 整数比较)
         ✗ → 跳过此 slot，检查下一个 tag 匹配的 slot
         ✓ → 调用外部传入的 key_cmp 闭包 (Step 5)

Step 5: Key 比较 — 外部闭包 (在 benchmark 的 run_taper 中定义)
         taper_hashmap.rs 不包含真正的列比较逻辑！
         它通过 key_cmp: Fn(&SlotValue) -> bool 委托给调用者。

         benchmark 中的 key_cmp 实现:
         ┌──────────────────────────────────────────────────────┐
         │ let key_cmp = |i: usize, sv: &SlotValue| -> bool {   │
         │     let rp = sv.get_ptr();  // 6B → 48-bit 地址      │
         │     for c in 0..num_cols {                            │
         │         let stored: i64 = unsafe {                    │
         │             (rp.add(col_offsets[c]) as *const i64)    │
         │                 .read_unaligned()                     │
         │         };                                            │
         │         if stored != data.keys[c][i] {                │
         │             return false;  // 列不匹配，early exit   │
         │         }                                            │
         │     }                                                │
         │     true  // 所有列都匹配                            │
         │ };                                                   │
         └──────────────────────────────────────────────────────┘

         这一步涉及:
         1. 解压 6B SlotValue → 指针
         2. 指针跳转到 RowContainer 的某一行 (可能 cache miss!)
         3. 按 col_offset 逐列读取并比较

         全部匹配 → on_update (累加 agg)
         不匹配 → 继续检查下一个 tag 匹配的 slot

Step 6: 无匹配 → 找 Empty Slot
         empty_mask = SWAR_match_empty(tags_u64)
         找到空位 → 写入 tag + hash + 分配新行 → on_init

Step 7: Chunk 满 → 线性探测
         pos = (pos + 1) & mask → 去下一个 Chunk 重复
```

### 10.6 SWAR BitMask 原理

```
目标: 同时检测 8 个字节中哪些等于 target

输入:  tags_u64 = [0x12, 0x3C, 0x80, 0x3C, 0x55, 0x80, 0x80, 0x80]
       target   = 0x3C

计算:
  broadcast = 0x01 * target = 0x3C3C3C3C3C3C3C3C
  x = tags XOR broadcast   = [0x2E, 0x00, 0xBC, 0x00, 0x69, 0xBC, 0xBC, 0xBC]
                                      ↑ zero        ↑ zero
  
  result = (x - 0x0101..01) & ~x & 0x8080..80
         → 只在"XOR 结果为 0"的字节位置设置 MSB

输出:  result 中 bit 7, bit 15, bit 23... 哪些为 1 → 对应 slot 匹配
       这里 slot 1 和 slot 3 匹配
```

### 10.7 Batch 处理 + Prefetch 流水线

```
emplace_batch_full 流程:

Round 1 (首轮):
  ┌─────────────────────────────────────────────────────────────┐
  │ rows:     [r0] [r1] [r2] ... [r15] [r16] [r17] ...         │
  │            ↓    ↓    ↓        ↓                             │
  │ prefetch: [p16][p17][p18]...[p31]   ← 提前 16 行 prefetch   │
  │            │                                                 │
  │ 处理 r0:  去 chunk[pos[0]] 做 emplace                       │
  │   成功 → 继续 r1                                            │
  │   chunk 满 → r0 加入 collision_buf                          │
  └─────────────────────────────────────────────────────────────┘

Round 2 (碰撞重试):
  ┌─────────────────────────────────────────────────────────────┐
  │ active = collision_buf (只剩碰撞的行)                        │
  │ 重新计算 pos: pos[r] = (原pos + collision_batch) & mask      │
  │ 重新 prefetch 新位置                                         │
  │ 再试一轮                                                     │
  └─────────────────────────────────────────────────────────────┘

Round 3, 4, ... 直到 collision_buf 为空

优势: 每次内存访问都被提前 prefetch，即使碰撞也不会 stall
```

---

## 11. 文件结构

```
benches/
  hashmap_bench.rs        ← benchmark 入口（本文档所述的全部逻辑）

src/
  taper_hashmap.rs        ← TaperHashMap 实现
  chunk.rs                ← Chunk + SlotValue 结构
  bitmask.rs              ← SWAR BitMask 实现
  row_container.rs        ← RowContainer（行式存储）
  batch_compare.rs        ← 批量比较工具

Cargo.toml                ← [[bench]] name="hashmap_bench", harness=false
```
