# Benchmark 原理图

## 整体结构

```
┌─────────────────────────────────────────────────────────────────┐
│                    Criterion Benchmark                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  数据生成 (一次性, 不计入测量时间):                                 │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │ KeyModel::generate("2col_i64", rows=1M, groups=100)      │  │
│  │   → col_a: Vec<i64> [100万个值, 100种不同]                 │  │
│  │   → col_b: Vec<i64> [100万个值, 100种不同]                 │  │
│  │   → hashes: Vec<u64> [100万个预计算hash]                   │  │
│  │   → values: Vec<i64> [100万个, 用于 sum 聚合]              │  │
│  └──────────────────────────────────────────────────────────┘  │
│                         │                                       │
│                         ▼                                       │
│  ┌──────────────┐   ┌──────────────┐                           │
│  │ bench "daft"  │   │ bench "taper" │  ← Criterion 分别测量    │
│  │ (重复 N 次)   │   │ (重复 N 次)   │                          │
│  └──────────────┘   └──────────────┘                           │
│         │                    │                                   │
│         ▼                    ▼                                   │
│  输出: time: [2.1ms 2.2ms 2.3ms]   time: [3.4ms 3.5ms 3.6ms]  │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

---

## Daft 侧一次迭代做了什么

```
b.iter(|| {                          ← Criterion 调用这个闭包 N 次并计时
    ┌──────────────────────────────────────────────────────┐
    │ 初始化:                                               │
    │   table = hashbrown::HashMap (空, 预分配)             │
    │   sums = Vec (空)                                    │
    │   ngroups = 0                                        │
    └──────────────────────────────────────────────────────┘
                    │
                    ▼
    ┌──────────────────────────────────────────────────────┐
    │ for i in 0..1_000_000:                                │
    │                                                      │
    │   h = hashes[i]                                      │
    │                                                      │
    │   entry = table.raw_entry_mut().from_hash(h, |other| {│
    │       (h == other.hash)              ← hash 比较      │
    │       && keys.compare(i, other.idx)  ← comparator    │
    │   })                                                 │
    │                                                      │
    │   ┌─ Occupied → gid = *e.get()                       │
    │   │                                                  │
    │   └─ Vacant → gid = ngroups++                        │
    │              insert IndexHash{idx:i, hash:h}         │
    │              sums.push(0)                             │
    │                                                      │
    │   sums[gid] += values[i]   ← 聚合                    │
    └──────────────────────────────────────────────────────┘
                    │
                    ▼
    black_box(&sums)   ← 防止编译器优化掉
})
```

**每行开销**: hashbrown tag match (NEON) + hash== + comparator(逐列) + sums[gid]++

---

## Taper 侧一次迭代做了什么

```
b.iter(|| {
    ┌──────────────────────────────────────────────────────┐
    │ 初始化:                                               │
    │   map = TaperHashMap (空, 预分配)                     │
    │   sums, group_rep_rows, new_entries, existing_entries │
    └──────────────────────────────────────────────────────┘
                    │
                    ▼
    ┌──────────────────────────────────────────────────────┐
    │ Phase 1: emplace_batch(hashes)                        │
    │                                                      │
    │ for i in 0..1_000_000:                                │
    │   tag = (hash >> 16) & 0x7F                          │
    │   chunk = chunks[hash & mask]                         │
    │   SWAR: match_tag(chunk.tags, tag)                   │
    │                                                      │
    │   ┌─ tag+hash 匹配 → on_existing:                    │
    │   │    read gid from SlotValue                       │
    │   │    push (i, gid) to existing_entries             │
    │   │                                                  │
    │   └─ empty slot → on_new:                            │
    │        write gid to SlotValue                        │
    │        push (i, gid) to new_entries                  │
    │        sums.push(0)                                  │
    │        group_rep_rows.push(i)                        │
    └──────────────────────────────────────────────────────┘
                    │
                    ▼
    ┌──────────────────────────────────────────────────────┐
    │ Phase 1.5: 处理 new_entries                           │
    │   for (idx, g) in new_entries:                        │
    │     sums[g] += values[idx]                           │
    └──────────────────────────────────────────────────────┘
                    │
                    ▼
    ┌──────────────────────────────────────────────────────┐
    │ Phase 2: deferred compare (existing_entries)          │
    │                                                      │
    │ for (idx, tentative_gid) in existing_entries:         │
    │   rep = group_rep_rows[tentative_gid]                │
    │   keys.compare(idx, rep)   ← 和 Daft 相同的 comparator│
    │   sums[tentative_gid] += values[idx]                 │
    └──────────────────────────────────────────────────────┘
                    │
                    ▼
    black_box(&sums)
})
```

**每行开销**:
- Phase 1: SWAR tag + hash== + SlotValue 读写 + Vec push
- Phase 2: comparator(逐列) + sums[gid]++

---

## 两侧的关键区别

```
时间线 (处理 100万行):

Daft:
  row 0: [tag SIMD + hash== + comparator + sum] 
  row 1: [tag SIMD + hash== + comparator + sum]
  row 2: [tag SIMD + hash== + comparator + sum]
  ...
  row 999999: [tag SIMD + hash== + comparator + sum]
  ↑ 每行都立即做完所有步骤

Taper:
  ═══ Phase 1 (100万行一口气) ═══
  row 0: [SWAR tag + hash== + SlotValue read/write]
  row 1: [SWAR tag + hash== + SlotValue read/write]
  ...
  row 999999: [SWAR tag + hash== + SlotValue read/write]
  ↑ 不做 comparator, 不做 sum

  ═══ Phase 2 (~999900 行 existing) ═══
  entry 0: [comparator + sum]
  entry 1: [comparator + sum]
  ...
  ↑ 统一做 comparator 和 aggregation
```

---

## Taper 理论优势

```
如果 comparator 贵 (string 比较):

  Daft: 100万次 × (tag + hash + COMPARATOR + sum)
                              ↑ 贵!

  Taper Phase 1: 100万次 × (tag + hash + SlotValue)  ← 便宜
        Phase 2: 100万次 × (COMPARATOR + sum)         ← 一样贵

  看起来一样? 不! 因为:
  - Phase 1 的 tight loop 更 cache 友好 (只读 hash+chunk, 不读 key 列)
  - Phase 2 顺序遍历 existing_entries, 访问模式可预测
  - 而 Daft 的 comparator 在 probe 循环内部, 破坏流水线
```

---

## Taper 实际劣势 (当前实现)

```
额外开销:
  - new_entries: Vec push × ~100 次 (新 group)
  - existing_entries: Vec push × ~999900 次 ← 大!
  - SlotValue 6-byte 读写 (比 hashbrown 直接存 u32 多一次间接)
  - dyn FnMut 回调不能内联
  - should_expand() 每行检查 (浮点比较)

hashbrown 的优势:
  - NEON SIMD (比 SWAR 快)
  - raw_entry_mut 闭包被单态化内联
  - 没有中间 Vec 分配
```

---

## 参数矩阵

### bench_key_complexity

```
           groups=10    groups=100   groups=1000   groups=10000
1col_i64   daft vs taper  ...          ...           ...
2col_i64   daft vs taper  ...          ...           ...
4col_i64   daft vs taper  ...          ...           ...
2col+str   daft vs taper  ...          ...           ...

rows = 1M (固定)
```

### bench_row_scale

```
           rows=100K    rows=1M     rows=10M
2col_i64   daft vs taper  ...        ...

groups = 100 (固定)
```

### bench_load_factor

```
           lf=0.5      lf=0.7      lf=0.9
2col_i64   daft vs taper  ...        ...

rows=1M, groups=1000 (固定)
```
