# Benchmark 代码逐行解释

本文档对 `benches/hashmap_bench.rs` 的每一段逻辑进行详细解释。

---

## 文件头注释

```rust
//! Hash table microbenchmark: TaperHashMap vs hashbrown (Daft-style)
//!
//! 纯 hash table 层面的 build + probe 性能测试 (2col_i64 key)
```
- 这是一个**微基准测试**，只测哈希表本身的性能，不包含完整的查询引擎流程。
- Key 是两列 i64（模拟 `GROUP BY col_a, col_b`）。

```rust
//! 参数:
//!   - HT size: 256, 1024, 4096, 16384 (hash table slot 数)
//!   - Load Factor: 0.5, 0.75 → num_keys = ht_size * lf
//!   - Selectivity: 0.1, 0.3, 0.5, 0.7, 0.9
//!     (probe_hits / num_probe_rows)
```
- **HT size**: 哈希表的 slot 总数，控制表的大小。
- **Load Factor**: 实际插入的 key 数 = ht_size × lf。0.75 意味着表 75% 满。
- **Selectivity**: probe 阶段中命中已有 group 的比例。0.9 = 90% hit，10% miss。

```rust
//! 流程:
//!   Build: 插入 num_keys 个唯一 key
//!   Probe: 1M 行，selectivity 比例 hit，其余 miss
```
- 先 build（建表），再 probe（查表+聚合）。两部分拼成一个连续的流。

---

## 依赖导入

```rust
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
```
- `criterion`: Rust 标准的统计基准测试框架。会多次运行 benchmark 并统计均值/方差。
- `black_box`: 阻止编译器优化掉计算结果（确保代码真正执行）。
- `BenchmarkId`: 给每个 benchmark case 一个唯一标识（如 `taper/ht=1024_lf=0.75_sel=0.5`）。

```rust
use hashbrown::{HashMap, hash_map::RawEntryMut};
```
- `hashbrown`: Rust 标准库 HashMap 的底层实现（Swiss table）。
- `RawEntryMut`: 低级 API，允许我们传入预计算的 hash 和自定义比较器，避免二次 hash。

```rust
use rand::{Rng, SeedableRng, rngs::StdRng};
```
- 固定 seed 的随机数生成器，确保每次运行数据一致。

```rust
use std::hash::{BuildHasherDefault, Hash, Hasher};
```
- 用于构建自定义的 `IdentityHasher`。

```rust
use taper_hashmap::chunk::SlotValue;
use taper_hashmap::row_container::RowContainer;
use taper_hashmap::taper_hashmap::TaperHashMap;
```
- 从我们的 crate 导入核心类型。

```rust
use xxhash_rust::xxh3::xxh3_64_with_seed;
```
- xxHash3：高性能 hash 函数，用于预计算两列 key 的组合 hash。

---

## Daft 基础设施

### IdentityHasher

```rust
#[derive(Default)]
struct IdentityHasher(u64);
impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 { self.0 }
    fn write(&mut self, _: &[u8]) { unreachable!() }
    fn write_u64(&mut self, i: u64) { self.0 = i; }
}
type IdentityBuildHasher = BuildHasherDefault<IdentityHasher>;
```
- **目的**: hashbrown 内部会对 key 调用 `hash()` 来计算桶位置。但我们已经预算好了 hash，不想再算一次。
- `IdentityHasher` 直接返回传入的 u64，不做任何变换。
- 这样 hashbrown 内部用的 hash 就是我们预计算的 hash 值，和 Taper 侧完全一致。

### IndexHash

```rust
#[derive(Eq, PartialEq)]
struct IndexHash { idx: u64, hash: u64 }
impl Hash for IndexHash {
    fn hash<H: Hasher>(&self, state: &mut H) { state.write_u64(self.hash); }
}
```
- **Key 类型**: 存两个字段:
  - `idx`: 行号（用来回查原始列数组做真正的 key 比较）
  - `hash`: 预计算的 hash 值
- `Hash` trait 只写 `self.hash`，配合 `IdentityHasher`，hashbrown 定位桶时用的就是预算好的 hash。

---

## Hash 函数

```rust
#[inline]
fn hash2(a: u64, b: u64) -> u64 {
    let h = xxh3_64_with_seed(&a.to_le_bytes(), 0);
    xxh3_64_with_seed(&b.to_le_bytes(), h)
}
```
- 两列 key 的组合 hash：先 hash 第一列（seed=0），结果作为第二列的 seed。
- 类似 boost::hash_combine 的思路：链式 hash 把多列合成一个 u64。
- **两边共用**：Daft 和 Taper 用同样的预计算 hash，确保对比公平。

---

## Taper 侧：`run_taper_hashagg`

```rust
#[inline(never)]
fn run_taper_hashagg(
    all_a: &[i64], all_b: &[i64], all_hashes: &[u64], all_values: &[i64],
    ht_size: usize,
) {
```
- `#[inline(never)]`: 阻止编译器把整个函数内联到 benchmark loop 里，确保性能统计准确。
- 参数：两列 key、预计算 hash、聚合值、哈希表大小。

```rust
    let mut rc = RowContainer::new(&[8, 8], 8);
    rc.reserve(ht_size + 64);
```
- `RowContainer::new(&[8, 8], 8)`: 两列 i64 key（各 8 字节）+ 8 字节聚合状态（SUM 的 i64）。
- `reserve`: 预分配内存，避免后续 `new_row()` 触发 realloc 导致指针失效。

```rust
    let mut map = TaperHashMap::with_capacity(ht_size);
```
- 创建指定 chunk 数的哈希表。

```rust
    let agg_offset = rc.agg_state_offset();
    let col0_offset = rc.column_at(0).offset;
    let col1_offset = rc.column_at(1).offset;
```
- 预先取出各字段在 row 中的字节偏移，避免循环内重复计算。
- `col0_offset`: 第一列值在 row 中的偏移（跳过 null byte）。
- `col1_offset`: 第二列值的偏移。
- `agg_offset`: 聚合状态的偏移。

### 主循环

```rust
    for (i, &h) in all_hashes.iter().enumerate() {
        let ka = all_a[i];
        let kb = all_b[i];
        let val = all_values[i];
```
- 逐行处理：取出当前行的两列 key 值、hash、和要聚合的值。

#### key_cmp 闭包

```rust
        let key_cmp = |sv: &SlotValue| -> bool {
            let rp = sv.get_ptr();
            let sa: i64 = unsafe { (rp.add(col0_offset) as *const i64).read_unaligned() };
            let sb: i64 = unsafe { (rp.add(col1_offset) as *const i64).read_unaligned() };
            sa == ka && sb == kb
        };
```
- 第三层比较：从 SlotValue 的 6B 指针中恢复出 row 指针。
- 从 row 中读出存储的两列 i64 值。
- 和当前行的 key 比较。如果都相等，返回 true（是同一个 group）。

#### on_init 闭包

```rust
        let mut on_init = |sv: &mut SlotValue| {
            let row = rc.new_row();
            unsafe {
                (row.add(col0_offset) as *mut i64).write_unaligned(ka);
                (row.add(col1_offset) as *mut i64).write_unaligned(kb);
                (row.add(agg_offset) as *mut i64).write_unaligned(val);
            }
            sv.set_ptr(row as *const u8);
        };
```
- 当找到空 slot（新 group）时调用。
- `rc.new_row()`: 从 RowContainer 分配一个新的零初始化行。
- 把两列 key 值写入 row 对应位置。
- 把聚合初始值写入 agg_state 位置。
- `sv.set_ptr(row)`: 把 row 指针压缩成 6 字节存入 SlotValue。

#### on_update 闭包

```rust
        let mut on_update = |sv: &SlotValue, is_new: bool| {
            if !is_new {
                let rp = sv.get_ptr() as *mut u8;
                unsafe { *(rp.add(agg_offset) as *mut i64) += val; }
            }
        };
```
- 每次 emplace 结束时调用（无论新建还是找到已有）。
- `is_new == true`: 刚 on_init 过了，聚合值已写入，不需要再加。
- `is_new == false`: 已有 group，通过指针直接在 row 上做 `SUM += val`。

#### emplace 调用

```rust
        map.emplace(h, &key_cmp, &mut on_init, &mut on_update);
    }
```
- 核心调用：传入 hash、三个回调。TaperHashMap 内部完成 tag→hash→key_cmp 三层过滤。

```rust
    black_box(rc.num_rows());
}
```
- 防止编译器优化掉整个函数（因为结果没有被使用）。

---

## Benchmark 主函数：`bench_build_probe`

```rust
fn bench_build_probe(c: &mut Criterion) {
    let mut group = c.benchmark_group("ht_probe");
    group.sample_size(30);
```
- 创建 benchmark 组，名字为 "ht_probe"。
- `sample_size(30)`: 每个 case 运行 30 次采样（criterion 默认 100，这里减少以加速）。

```rust
    let num_probe_rows = 1_000_000;
```
- Probe 阶段固定 1M 行。

### 三层循环：ht_size × load_factor × selectivity

```rust
    for &ht_size in &[256, 1024, 4096, 16384] {
        for &load_factor in &[0.5, 0.75] {
            let num_keys = (ht_size as f64 * load_factor) as usize;
```
- `num_keys`: 实际要插入的唯一 key 数量。例如 ht_size=1024, lf=0.75 → 768 个唯一 key。

```rust
            for &selectivity in &[0.1, 0.3, 0.5, 0.7, 0.9] {
                let mut rng = StdRng::seed_from_u64(42);
```
- 每组参数重置随机数种子，确保可复现。

### 数据生成：Build Keys

```rust
                let build_a: Vec<i64> = (0..num_keys).map(|i| i as i64 * 97 + 1).collect();
                let build_b: Vec<i64> = (0..num_keys).map(|i| i as i64 * 53 + 7).collect();
```
- 确定性生成唯一的 key 对：`(97*i+1, 53*i+7)`。不会重复。

```rust
                let build_hashes: Vec<u64> = (0..num_keys)
                    .map(|i| hash2(build_a[i] as u64, build_b[i] as u64)).collect();
```
- 预计算每个 build key 的 hash。

```rust
                let build_values: Vec<i64> = (0..num_keys).map(|i| (i % 1000) as i64).collect();
```
- Build 阶段的聚合值（0~999 循环）。

### 数据生成：Probe Keys

```rust
                let num_hits = (num_probe_rows as f64 * selectivity) as usize;
                let num_misses = num_probe_rows - num_hits;
```
- 根据 selectivity 计算 hit/miss 数量。

```rust
                // Hits: random from build keys
                for _ in 0..num_hits {
                    let idx = rng.random_range(0..num_keys);
                    probe_a.push(build_a[idx]);
                    probe_b.push(build_b[idx]);
                    probe_hashes.push(build_hashes[idx]);
                }
```
- Hit 行：从已有 build keys 中随机选取。这些行一定能在表中找到对应 group。

```rust
                // Misses: guaranteed not in build keys
                let miss_base = (num_keys as i64 + 1) * 97 + 10000;
                for i in 0..num_misses {
                    let a = miss_base + i as i64 * 31;
                    let b = miss_base + i as i64 * 17 + 3;
                    probe_a.push(a);
                    probe_b.push(b);
                    probe_hashes.push(hash2(a as u64, b as u64));
                }
```
- Miss 行：用远离 build key 范围的值生成，保证不在表中。
- 这些行会触发 "创建新 group" 的路径。

```rust
                // Shuffle to interleave hits and misses
                let mut order: Vec<usize> = (0..num_probe_rows).collect();
                for i in (1..num_probe_rows).rev() {
                    order.swap(i, rng.random_range(0..=i));
                }
                let probe_a: Vec<i64> = order.iter().map(|&i| probe_a[i]).collect();
                let probe_b: Vec<i64> = order.iter().map(|&i| probe_b[i]).collect();
                let probe_hashes: Vec<u64> = order.iter().map(|&i| probe_hashes[i]).collect();
```
- Fisher-Yates 洗牌：把 hit 和 miss 打乱顺序，模拟真实查询中 hit/miss 随机交替的情况。
- 如果不打乱，所有 hit 连续出现会让 branch predictor 过于有效，结果不真实。

```rust
                // Combine build + probe into single stream
                let mut all_a = build_a.clone();
                all_a.extend_from_slice(&probe_a);
                // ... all_b, all_hashes, all_values 同理
```
- 把 build keys 和 probe keys 拼成一个连续流。
- 模拟 hash aggregation 的真实模式：先遇到的行建 group，后面的行要么 hit 已有 group 要么建新的。

### Daft Benchmark

```rust
                group.bench_with_input(
                    BenchmarkId::new("daft", &param),
                    &(&all_a, &all_b, &all_hashes, &all_values),
                    |b, &(aa, ab, ah, av)| {
```
- `bench_with_input`: criterion 会多次调用闭包里的 `b.iter()` 来统计性能。
- 输入数据在外部准备好，通过引用传入，不计入测量时间。

```rust
                        b.iter(|| {
                            let mut table = HashMap::<IndexHash, u32, IdentityBuildHasher>
                                ::with_capacity_and_hasher(ht_size, Default::default());
                            let mut ngroups: u32 = 0;
                            let mut sums = Vec::<i64>::with_capacity(ht_size);
```
- 每次迭代重新建表（测量包含 build 和 probe 的完整流程）。
- `table`: key=IndexHash, value=u32(group_id), hasher=IdentityHasher。
- `sums`: 聚合结果数组，通过 group_id 索引。

```rust
                            for (i, &h) in ah.iter().enumerate() {
                                let entry = table.raw_entry_mut().from_hash(h, |other| {
                                    (h == other.hash) && {
                                        let j = other.idx as usize;
                                        aa[i] == aa[j] && ab[i] == ab[j]
                                    }
                                });
```
- `raw_entry_mut().from_hash(h, comparator)`:
  - 用预计算的 hash `h` 定位桶。
  - comparator 闭包：先比 hash 是否相等（快速路径），再通过行号 `j` 回查原始列数组比较真实 key。
  - 这**和 Daft 源码的逻辑一致**：Daft 的 hash table 存的是行号，比较时回到列数组逐列对比。

```rust
                                match entry {
                                    RawEntryMut::Occupied(e) => {
                                        sums[*e.get() as usize] += av[i];
                                    }
```
- **Hit**: 找到已有 group → 取出 group_id → `sums[gid] += val`。

```rust
                                    RawEntryMut::Vacant(e) => {
                                        e.insert_hashed_nocheck(h,
                                            IndexHash { idx: i as u64, hash: h },
                                            ngroups,
                                        );
                                        ngroups += 1;
                                        sums.push(av[i]);
                                    }
                                }
                            }
```
- **Miss**: 创建新 group → 插入 (IndexHash, group_id) → `sums.push(val)`。
- `insert_hashed_nocheck`: 告诉 hashbrown "我已经确认这个 hash 对应的桶是空的，直接插入，不要再 hash 一次"。

```rust
                            black_box(&sums);
                        });
```
- 防止编译器把整个循环优化掉。

### Taper Benchmark

```rust
                group.bench_with_input(
                    BenchmarkId::new("taper", &param),
                    &(&all_a, &all_b, &all_hashes, &all_values),
                    |b, &(aa, ab, ah, av)| {
                        b.iter(|| {
                            run_taper_hashagg(
                                black_box(aa), black_box(ab), black_box(ah), black_box(av),
                                ht_size,
                            );
                        });
                    },
                );
```
- 用相同的输入数据调用 Taper 版本。
- `black_box` 包裹输入引用，防止编译器在 benchmark 外预计算。

---

## 数据流总结

```
┌────────────────────────────────────────────────────────────────────┐
│                     数据准备 (不计入测量)                           │
│                                                                    │
│  build_keys (num_keys 个唯一 key)                                  │
│       +                                                            │
│  probe_keys (1M 行, selectivity 比例 hit, 打乱)                    │
│       ↓                                                            │
│  all_a[], all_b[], all_hashes[], all_values[]                      │
└────────────────────────────────────────────────────────────────────┘
                              ↓
┌───────────────────┐                    ┌───────────────────┐
│     Daft 版本      │                    │    Taper 版本      │
│                   │                    │                   │
│ hashbrown Swiss   │                    │ TaperHashMap      │
│ table + sums[]   │                    │ + RowContainer    │
│                   │                    │                   │
│ key: IndexHash    │                    │ key: hash in slot │
│ val: group_id     │                    │ val: 6B ptr→row   │
│ agg: sums[gid]   │                    │ agg: row内agg区    │
└───────────────────┘                    └───────────────────┘
         ↓                                        ↓
    criterion 统计对比 (ns/iter, throughput)
```

---

## 关键设计决策

1. **预计算 hash**: 两边用同一个 `hash2()` 函数预算好 hash 再传入，确保对比的是**纯哈希表操作性能**，不受 hash 函数本身性能影响。

2. **IdentityHasher**: 让 hashbrown 不做二次 hash，直接用预算好的值定位桶。否则 hashbrown 内部会再做一次 hash，不公平。

3. **Daft comparator 回查原数组**: 和 Daft 实际实现一致——table 里只存行号，比较时回到原始列数组按行号取值比对。

4. **Taper 存值在 row**: key 和 agg_state 紧凑存在同一行，比较和聚合都通过指针直接操作 row 内存。

5. **Shuffle**: hit/miss 打乱确保分支预测器不能作弊，模拟真实负载。

6. **Build+Probe 一体流**: 不分阶段，模拟 streaming hash aggregation（数据来一行处理一行）。
