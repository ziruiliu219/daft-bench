# Taper Hash Table — Group-By 聚合完整流程

## 概述

Taper Hash Table 是为数据库 GROUP BY 聚合场景设计的 hash table，核心思想是 **"先粗后细，先批后单"**：

1. 用廉价的 hash 比较快速分流大部分行（batch）
2. 再用精确的 key 比较验证少数可能碰撞的行（batch compare）
3. 极少数真正碰撞的行才走代价高的单行修复

等价 SQL: `SELECT key_cols..., SUM(val) FROM table GROUP BY key_cols...`

---

## 数据结构

```
┌─────────────────────────────────────────────────────────────────┐
│ TaperHashMap                                                     │
│                                                                  │
│ chunks: Vec<Chunk>    (每个 chunk 128B = 1 cache line)           │
│ ┌──────────────────────────────────────────────────────────────┐ │
│ │ Chunk (128 bytes, aligned)                                    │ │
│ │ ┌────────┬──────────────────────┬────────────────────┬─────┐ │ │
│ │ │tags 8B │   keys 64B (8×u64)   │ values 48B (8×6B)  │pad  │ │ │
│ │ └────────┴──────────────────────┴────────────────────┴─────┘ │ │
│ │                                                               │ │
│ │ tags[i]: 0x80=空, 其他=hash的7-bit摘要                        │ │
│ │ keys[i]: 完整64-bit hash值                                    │ │
│ │ values[i]: 6字节压缩指针 → 指向 RowContainer 的 row           │ │
│ └──────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────────┐
│ RowContainer                                                     │
│                                                                  │
│ pool: Vec<u8>   (连续内存，所有 group row 顺序排列)              │
│                                                                  │
│ 每行布局 (以 2col_i64 + sum 为例, 共 26 字节):                   │
│ ┌──────┬────────┬──────┬────────┬──────────┐                    │
│ │null 1B│ key0 8B│null 1B│ key1 8B│ sum 8B  │                    │
│ └──────┴────────┴──────┴────────┴──────────┘                    │
│ offset: 0   1        9   10       18                             │
└─────────────────────────────────────────────────────────────────┘
```

---

## 完整流程图

```
输入: col_a[], col_b[], values[], hashes[] (预计算的hash值)
      │
      ▼
╔═════════════════════════════════════════════════════════════════╗
║ 初始化                                                          ║
║                                                                 ║
║   RowContainer::new(&[8,8], 8)   ← 2列i64 key + 8字节 sum      ║
║   rc.reserve(num_groups + 64)    ← 预分配，避免realloc导致指针失效║
║   TaperHashMap::with_capacity(init_cap)                         ║
╚═════════════════════════════════════════════════════════════════╝
      │
      ▼
╔═════════════════════════════════════════════════════════════════╗
║ Stage 1: Batch Emplace                                          ║
║                                                                 ║
║ map.emplace_batch(hashes, on_new, on_existing)                  ║
║                                                                 ║
║ 对每个 hash:                                                     ║
║   ① pos = hash & mask                → 定位 chunk               ║
║   ② tag = (hash >> 16) & 0x7F        → 计算 7-bit tag           ║
║   ③ tags_u64 = 加载 8 个 tag 为 u64                             ║
║   ④ SWAR match_tag(tags, tag)        → 哪些 slot tag 匹配?     ║
║   ⑤ 匹配的 slot: 比较 keys[slot] == hash (u64整数比较)          ║
║      ├─ 相等 → on_existing(row_idx, slot.value)                 ║
║      │         记录到 existing_entries[(row_idx, row_ptr)]       ║
║      └─ 不等 → 继续找下一个匹配 slot                            ║
║   ⑥ 没匹配 → SWAR match_empty(tags)  → 找空 slot               ║
║      └─ 找到 → 写入 tag + hash                                  ║
║               → on_new(row_idx, &mut slot.value)                ║
║                 · rc.new_row()         分配新行                  ║
║                 · write_keys_to_row()  写入 key 列               ║
║                 · sv.set_ptr(row)      存指针到 SlotValue        ║
║                 · 记录到 new_entries[(row_idx, row_ptr)]         ║
║   ⑦ chunk 满 → pos = (pos + 1) & mask → 下一个 chunk, 重复③    ║
║                                                                 ║
║ 注意: 只比较 hash 值(u64), 不比较真实 key!                       ║
╚═════════════════════════════════════════════════════════════════╝
      │
      │ 产出: new_entries (确定新group)
      │       existing_entries (hash匹配，待验证key)
      ▼
╔═════════════════════════════════════════════════════════════════╗
║ Stage 1.5: 新 Group 聚合                                        ║
║                                                                 ║
║ for (idx, row) in new_entries:                                   ║
║     row.agg_state = values[idx]    // 第一行值直接写入           ║
╚═════════════════════════════════════════════════════════════════╝
      │
      ▼
╔═════════════════════════════════════════════════════════════════╗
║ Stage 2: Batch Key Compare                                      ║
║                                                                 ║
║ 目的: 验证 existing_entries 中每行的 key 是否真的等于            ║
║       它 hash 匹配到的 group 的 key                              ║
║                                                                 ║
║ 构建:                                                            ║
║   indices[0..N] = [0, 1, 2, ..., N-1]                           ║
║   group_ptrs[i] = existing_entries[i] 的 row 指针               ║
║   existing_row_indices[i] = existing_entries[i] 的输入行号       ║
║                                                                 ║
║ 逐列比较 (以 2 列 i64 为例):                                    ║
║                                                                 ║
║ ┌─ 第1列 col_a ─────────────────────────────────────────────┐   ║
║ │ input_a[i] = col_a[existing_row_indices[i]]               │   ║
║ │ compare_i64(indices[0..], N, input_a, group_ptrs, offset0)│   ║
║ │   → NEON: 每次加载2行，vceqq_s64 并行比较                 │   ║
║ │   → 不匹配的 swap 到 indices 前面                          │   ║
║ │   → 返回 new_mismatches                                    │   ║
║ │ mismatches += new_mismatches                               │   ║
║ └────────────────────────────────────────────────────────────┘   ║
║                                                                 ║
║ ┌─ 第2列 col_b (只处理通过col_a的行) ───────────────────────┐   ║
║ │ compare_i64(indices[mismatches..], N-mismatches, ...)      │   ║
║ │   → 只比"还没被淘汰"的行                                   │   ║
║ │ mismatches += new_mismatches                               │   ║
║ └────────────────────────────────────────────────────────────┘   ║
║                                                                 ║
║ (如果有 string 列: 对剩余行标量逐个比字符串)                    ║
║                                                                 ║
║ 最终结果:                                                        ║
║   indices[0..mismatches]         = 不匹配 (hash collision)      ║
║   indices[mismatches..N]         = 匹配 (key 确认相等)          ║
╚═════════════════════════════════════════════════════════════════╝
      │                                    │
      ▼                                    ▼
╔══════════════════════════╗   ╔═══════════════════════════════════╗
║ 匹配行: 聚合              ║   ║ 不匹配行: Collision Repair         ║
║                          ║   ║                                   ║
║ for i in mismatches..N:  ║   ║ for i in 0..mismatches:           ║
║   row_ptr.sum += val     ║   ║   map.emplace(hash,               ║
║                          ║   ║     key_cmp: 逐列比较所有 key,    ║
║ (通过指针直接写           ║   ║     on_new: 新建 row + 写key + agg║
║  RowContainer 的          ║   ║     on_match: sum += val          ║
║  agg_state 区域)          ║   ║   )                               ║
╚══════════════════════════╝   ╚═══════════════════════════════════╝
```

---

## 数据量分布（典型场景: 1M 行, 1000 groups, xxh3 hash）

| 阶段 | 处理行数 | 占比 | 说明 |
|------|---------|------|------|
| Stage 1: emplace_batch | 1,000,000 | 100% | 全量过 hash table |
| → new_entries | ~1,000 | 0.1% | 每个 group 仅首次出现时 |
| → existing_entries | ~999,000 | 99.9% | hash 匹配已有 group |
| Stage 2: batch compare | ~999,000 | 99.9% | 批量验证 key |
| → 匹配 (聚合) | ~999,000 | ≈100% | 几乎全部通过 |
| → collision (repair) | ~0 | ≈0% | xxh3 碰撞率极低 |

---

## 关键优化点

### 1. Stage 1 只做整数比较
- 不访问原始 key 数据（可能是多列、含字符串）
- 只比较 u64 hash 值，一次 `==` 搞定
- 大部分行在这里就完成了分流

### 2. SWAR 并行 tag 匹配
- 8 个 tag 打包成 u64，一次位运算比较全部
- 不需要 SIMD 指令，纯标量操作
- 快速排除不匹配的 slot

### 3. Stage 2 批量比较 + 逐列过滤
- `compare_i64` 用 NEON 一次比 2 行（ARM）
- 多列 key 逐列过滤：每列只处理"上一列通过"的行
- 列越多，后面列要处理的行越少

### 4. Collision Repair 几乎不触发
- xxh3 的碰撞率约 2^-64
- 1000 groups → 预期碰撞数 ≈ 0
- 即使触发，用单行 `emplace` 带完整 key 比较兜底

### 5. 内存局部性
- RowContainer: 所有 group 连续存放
- Chunk: 128 字节对齐 = 1 cache line
- 聚合写入: 通过指针直接写，无需二次查找

---

## 与 Daft (hashbrown) 对比

| | Daft (hashbrown) | Taper |
|---|---|---|
| 处理模型 | 逐行: probe + key compare + aggregate | 分阶段: batch emplace → batch compare → aggregate |
| Key 比较时机 | 每行 probe 时立即比完整 key | 推迟到 Stage 2 批量做 |
| 比较方式 | 闭包内标量逐列比较 | NEON/SWAR 批量比较 |
| Hash table | Swiss Table (hashbrown) | Chunked open-addressing (8-slot per chunk) |
| 聚合位置 | probe 循环内 | 独立阶段，批量写 |
| 额外内存 | 无 | RowContainer 存 group key + agg state |
| 优势场景 | 简单、通用、低 overhead | 高吞吐 batch 处理，key 比较代价高时优势大 |

---

## 代码位置

| 组件 | 文件 | 职责 |
|------|------|------|
| TaperHashMap | `src/taper_hashmap.rs` | hash table 核心: chunk 管理、emplace、probe、expand |
| Chunk | `src/chunk.rs` | 128B 对齐的 8-slot 存储单元 |
| BitMask | `src/bitmask.rs` | SWAR 位运算: tag 匹配、空 slot 查找 |
| RowContainer | `src/row_container.rs` | 行式内存池: 存 group key + agg state |
| compare_i64 | `src/batch_compare.rs` | NEON/标量 批量 key 比较 |
| hash_i64 | `src/hash.rs` | CRC32 硬件加速 hash（备选，benchmark 用 xxh3） |
| run_taper_pipeline | `benches/hashmap_bench.rs` | 完整流程编排 |
