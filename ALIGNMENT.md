# TaperHashTable 组件对齐文档

## C++ (OmniOperator) vs Rust (副本5) 实现对比

---

## 1. 整体架构

```
┌─────────────────────────────────────────────────────────────┐
│                    上层调用者 (GroupBy / Benchmark)            │
│                                                              │
│  hash 计算 → HashTable.emplace() → RowContainer.new_row()   │
└─────────────────────────────────────────────────────────────┘
         │                              │
         ▼                              ▼
┌──────────────────────┐    ┌──────────────────────────┐
│   TaperHashTable     │    │      RowContainer        │
│   (快速查找/插入)     │    │   (行数据存储)            │
│                      │    │                          │
│  ┌────────────────┐  │    │  行布局:                  │
│  │    Chunk[]     │  │    │  [keys][null block][agg] │
│  │  128B aligned  │──┼───→│                          │
│  └────────────────┘  │    │                          │
│                      │    │                          │
│  ┌────────────────┐  │    │  ┌────────────────────┐  │
│  │   BitMask      │  │    │  │    RowColumn       │  │
│  │  SWAR 匹配     │  │    │  │  列描述符(packed)   │  │
│  └────────────────┘  │    │  └────────────────────┘  │
└──────────────────────┘    └──────────────────────────┘
```

两者是独立组件，通过回调函数(callback)连接。HashTable 不依赖 RowContainer，反之亦然。

---

## 2. 组件详细对比

### 2.1 Chunk (哈希桶)

| 属性 | C++ `TaperHashTableChunk` | Rust `Chunk` | 对齐 |
|------|---------------------------|--------------|------|
| 大小 | 128 bytes | 128 bytes | ✅ |
| 对齐 | 128-byte aligned | `#[repr(C, align(128))]` | ✅ |
| Slots 数量 | 8 (动态计算，key=8B+val=6B 时为 8) | 8 (`SLOTS_PER_CHUNK`) | ✅ |
| Tags 位置 | offset 0, 8 bytes | offset 0, 8 bytes | ✅ |
| Keys 位置 | offset 8 (`(8+7)&0xF8=8`), 64B | offset 8, 64B (`[u64; 8]`) | ✅ |
| Values 位置 | offset 80 (`(8+64+15)&0xF0=80`), 48B | offset 80 (8B pad between keys and values), 48B | ✅ |
| Key 类型 | `int64_t` (hash value, `KeyScattered=true`) | `u64` (hash value) | ✅ |
| Value 类型 | `FixedBuf` (6B, 存 RowContainer 行指针) | `SlotValue` (6B, 存 RowContainer 行指针) | ✅ |
| Empty tag | `0x80` | `0x80` | ✅ |
| Tags 批量读取 | `GetU64Tags()` → `*(uint64_t*)buf` | `tags_u64()` → `u64::from_le_bytes` | ✅ |

**C++ 源码:** `taper_hashtable.h` 第 128-145 行 (`TaperHashTableChunk`)
**Rust 源码:** `chunk.rs`

---

### 2.2 BitMask (SWAR 位掩码匹配)

| 属性 | C++ `PHBitMask` | Rust `BitMask` | 对齐 |
|------|-----------------|----------------|------|
| Tag 匹配公式 | `(x - kLsbs) & ~x & kMsbs` | `(x.wrapping_sub(LSBS)) & !x & MSBS` | ✅ |
| Empty 匹配公式 | `(tags & (~tags << 7)) & msbs` | `(tags & (!tags << 7)) & MSBS` | ✅ |
| kMsbs | `0x8080808080808080` | `0x8080808080808080` | ✅ |
| kLsbs | `0x0101010101010101` | `0x0101010101010101` | ✅ |
| 迭代方式 | `++iterator` (clear LSB) | `Iterator::next()` (advance = self & (self-1)) | ✅ |
| Slot 索引提取 | `__builtin_ctzll(mask) >> 3` | `trailing_zeros() >> 3` | ✅ |

**C++ 源码:** `taper_hashtable.h` 第 54-105 行 (`PHBitMask`)
**Rust 源码:** `bitmask.rs`

---

### 2.3 TaperHashTable (哈希表主体)

#### 2.3.1 核心参数

| 参数 | C++ | Rust | 对齐 |
|------|-----|------|------|
| Load factor | 0.9 (`capacity - capacity/10`) | 0.9 (`LOAD_FACTOR_THRESHOLD`) | ✅ |
| Prefetch 距离 | 16 (`kHashMapPrefetchDist`) | 16 (`PREFETCH_DIST`) | ✅ |
| Chunk 定位 | `hashVal & lastChunkIdx_` | `hash as usize & self.mask` | ✅ |
| 碰撞探查 | `(pos + collisionBatch) & lastChunkIdx_` | `(pos + collision_batch) & self.mask` | ✅ |
| Tag hash 计算 | `(hashVal >> 16) & 0x7F` | `((hash >> 16) & 0x7F) as u8` | ✅ |
| 容量必须 2 的幂 | `IsValidCapacity(n)` | `.next_power_of_two()` | ✅ |

#### 2.3.2 Emplace (逐行插入)

| 步骤 | C++ `EmplaceImpl` + `TryEmplaceAtPos` | Rust `emplace` | 对齐 |
|------|---------------------------------------|-----------------|------|
| 1. 扩容检查 | `ShouldExpand()` → `ExpandCapacityDirectly()` | `should_expand()` → `expand()` | ✅ |
| 2. 定位 chunk | `GetChunkPos(hashVal)` | `chunk_pos(hash)` | ✅ |
| 3. 读 tags u64 | `curChunk->GetU64Tags()` | `chunk.tags_u64()` | ✅ |
| 4. Tag match | `PHBitMask::MatchTag(tags, tagHash)` | `BitMask::match_tag(tags, tag_hash)` | ✅ |
| 5. Key 比较 | `fKeyCmp(key, *curChunk, i)` | `key_cmp(&chunk.values[slot])` | ✅ |
| 6. Empty match | `PHBitMask::MatchEmpty(tags, emptyTags_)` | `BitMask::match_empty(tags)` | ✅ |
| 7. 写入新 slot | `SetChunkKey` + `fInit` + `fUpdate` | `tags[s]=tag; keys[s]=hash; on_init; on_update` | ✅ |
| 8. Chunk 满重试 | `GetRehashPos(batch, pos)` | `rehash_pos(batch, pos)` | ✅ |

**C++ 源码:** `taper_hashtable.h` 第 453-498 行 (`EmplaceImpl`), 第 1079-1112 行 (`TryEmplaceAtPos`)
**Rust 源码:** `taper_hashmap.rs` `emplace` 方法

#### 2.3.3 Batch Emplace (批量插入 + Collision Retry)

| 步骤 | C++ `EmplaceBatchImpl` | Rust `emplace_batch_full` | 对齐 |
|------|------------------------|---------------------------|------|
| 1. 预计算所有 hash+positions | `ResetEmplaceContext(keys, numRows)` | `positions = hashes.map(chunk_pos)` | ✅ |
| 2. Prefetch 前 16 个 | `HwpPrefetch(chunkPositions)` | `prefetch_chunk(positions[0..16])` | ✅ |
| 3. 主循环逐行处理 | `for(i=0; i<numRows; i++)` | `for idx in 0..count` | ✅ |
| 4. 循环内 prefetch+16 | `prefetchIdx(positions, i, numRows)` | `prefetch_chunk(positions[active[pi]])` | ✅ |
| 5. 碰撞收集 | `collisionIndices[count++] = rowIdx` | `collision_buf.push(row_idx)` | ✅ |
| 6. 碰撞行更新位置 | `GetRehashPos(batch, pos)` | `rehash_pos(collision_batch, pos)` | ✅ |
| 7. 迭代重试循环 | `while(collisionCount > 0)` | `loop { if collision_buf.is_empty() break }` | ✅ |
| 8. 每轮重试 prefetch | `HwpPrefetch` + `prefetchIdx` | `prefetch_chunk` | ✅ |

**C++ 源码:** `taper_hashtable.h` 第 522-644 行
**Rust 源码:** `taper_hashmap.rs` `emplace_batch_full` 方法

#### 2.3.4 Prefetch (软件预取)

| 属性 | C++ | Rust | 对齐 |
|------|-----|------|------|
| 指令 (x86_64) | `__builtin_prefetch` → `prefetcht0` | `asm!("prefetcht0 [{ptr}]")` | ✅ |
| 指令 (AArch64) | `__builtin_prefetch` → `prfm pldl1keep` | `asm!("prfm pldl1keep, [{ptr}]")` | ✅ |
| 每 chunk 2 条 | `prefetch(chunk)` + `prefetch(chunk+64)` | `prefetch_read(ptr)` + `prefetch_read(ptr+64)` | ✅ |
| 提前距离 | 16 | 16 | ✅ |

**C++ 源码:** `taper_hashtable.h` 第 260-275 行
**Rust 源码:** `taper_hashmap.rs` `prefetch_chunk` + `prefetch_read`

#### 2.3.5 Expand/Rehash (扩容)

| 步骤 | C++ `RehashBatch` + `RehashChunksIteratively` | Rust `expand` | 对齐 |
|------|-----------------------------------------------|---------------|------|
| 1. 分配 2x 新表 | `Init(ExpandLastChunkIdx())` | `chunks = (0..new_len).map(Chunk::new)` | ✅ |
| 2. 收集旧元素 | Visitor 遍历旧 chunks | 遍历 `old_chunks` 收集 `entries` | ✅ |
| 3. 预计算新位置 | `ResetRehashContext` | `positions = entries.map(chunk_pos)` | ✅ |
| 4. Prefetch 驱动插入 | `prefetchIdx` in loop | `prefetch_chunk` in loop | ✅ |
| 5. Insert-only (无 key cmp) | `TryEmplaceAtPos<true>` (skip MatchTag) | `match_empty` 直接写入 | ✅ |
| 6. 碰撞迭代重试 | `while(collisionCount > 0)` | `loop { if count == 0 break }` | ✅ |
| 7. 释放旧表 | `FreeChunkMemory(oldChunks)` | `old_chunks` dropped (Rust RAII) | ✅ |

**C++ 源码:** `taper_hashtable.h` 第 670-740 行, 第 1135-1147 行
**Rust 源码:** `taper_hashmap.rs` `expand` 方法

---

### 2.4 RowContainer (行存储)

#### 2.4.1 行布局

| 属性 | C++ | Rust | 对齐 |
|------|-----|------|------|
| 布局 | `[key0][key1]...[null_block][agg_state]` | `[key0][key1]...[null_block][agg_state]` | ✅ |
| Key 紧凑排列 | 无间隙 | 无间隙 | ✅ |
| Null block 位置 | 紧跟最后一个 key | `null_block_start = sum(key_sizes)` | ✅ |
| Null block 大小 | `ceil(numKeys / 8)` bytes | `(num_keys + 7) / 8` bytes | ✅ |
| Null 编码 | 1 bit per column, packed | 1 bit per column, packed | ✅ |
| AggState 位置 | `null_block_start + null_bytes` | `null_block_start + null_bytes` | ✅ |

#### 2.4.2 RowColumn (列描述符)

| 属性 | C++ `RowColumn` | Rust `RowColumn` | 对齐 |
|------|-----------------|------------------|------|
| 存储 | 单个 `uint64_t` packed | 单个 `u64` packed | ✅ |
| Offset 提取 | `packedOffsets >> 32` | `packed >> 32` | ✅ |
| NullByte 提取 | `(uint32_t)(packedOffsets) >> 8` | `(packed >> 8) & 0x00FF_FFFF` | ✅ |
| NullMask 提取 | `packedOffsets & 0xff` | `packed & 0xFF` | ✅ |

#### 2.4.3 内存管理

| 属性 | C++ | Rust | 对齐 |
|------|-----|------|------|
| 分配策略 | Arena batch (kBatchSize=1024) | Block-based (BLOCK_ROWS=1024) | ✅ |
| 指针稳定性 | Arena 保证不移动 | 独立 Vec 堆分配不移动 | ✅ |
| 零初始化 | `memset(0)` | `vec![0u8; ...]` | ✅ |

#### 2.4.4 静态方法

| 方法 | C++ | Rust | 对齐 |
|------|-----|------|------|
| 读值 | `ReadValue<T>(row, offset)` | `read_value::<T>(row, offset)` | ✅ |
| 写值 | `StoreValue(row, offset, val)` | `store_value::<T>(row, offset, val)` | ✅ |
| 判空 | `IsNullAt(row, nullByte, nullMask)` | `is_null_at(row, null_byte, null_mask)` | ✅ |
| 设空 | `SetNullAt(row, nullByte, nullMask)` | `set_null_at(row, null_byte, null_mask)` | ✅ |
| 清空 | `ClearNullAt(row, nullByte, nullMask)` | `clear_null_at(row, null_byte, null_mask)` | ✅ |
| 列描述 | `ColumnAt(colIdx)` | `column_at(col_idx)` | ✅ |

**C++ 源码:** `row_container.h`
**Rust 源码:** `row_container.rs`

---

## 3. 未实现的 C++ 功能 (不影响 benchmark)

以下是 C++ 有但 Rust 未实现的功能，因为在纯 hash table benchmark 场景中不需要：

| 功能 | 说明 | 原因 |
|------|------|------|
| `ListRows` / `RowContainerIterator` | 遍历所有已分配行 | benchmark 不需要结果遍历 |
| `ExtractColumn` | 从行中提取列到向量 | benchmark 不需要结果输出 |
| `Equals` | 行与向量值比较 | benchmark 不需要 |
| Free list (行回收) | `firstFreeRow` / `numFreeRows` | benchmark 不删除行 |
| `Reset()` | 批量重置容器 | benchmark 不复用容器 |
| HWP (Hardware Prefetch Engine) | ARM Kunpeng Link RPRFM | 鲲鹏专用硬件功能 |
| `EmplaceBatch` 中途扩容处理 | 批量插入时边扩容边 rehash 未完成行 | benchmark 容量预留足够 |

---

## 4. 文件对照表

| C++ 文件 | Rust 文件 | 内容 |
|----------|-----------|------|
| `taper_hashtable.h` (全部) | `taper_hashmap.rs` | 哈希表主体 |
| `taper_hashtable.h` (`TaperHashTableChunk`) | `chunk.rs` | Chunk 数据结构 |
| `taper_hashtable.h` (`PHBitMask`) | `bitmask.rs` | SWAR 位匹配 |
| `row_container.h` | `row_container.rs` | 行存储 |
| N/A (上层调用者) | `benches/hashmap_bench.rs` | Benchmark |

---

## 5. 结论

Rust 副本5 的 TaperHashTable 实现在以下维度与 C++ 原版**完全一致**：

1. **数据结构布局** — Chunk 128B 对齐、内部偏移、RowContainer 行布局
2. **核心算法** — SWAR tag match、开放寻址线性探查、0.9 load factor 扩容
3. **性能优化** — Software prefetch (距离16)、batch collision retry with prefetch、iterative batch rehash
4. **接口语义** — emplace 回调模式 (key_cmp / on_init / on_update)

Benchmark 结果可以公平反映两种语言在相同算法下的性能差异。
