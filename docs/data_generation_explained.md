# 副本2 Benchmark 数据生成逻辑详解

## 核心问题

这个 benchmark 模拟的就是 `SELECT col_a, col_b, SUM(val) GROUP BY col_a, col_b`。

Hash agg 的本质：每来一行，查 hash table：
- 找到了 → 更新该 group 的 sum（**probe hit**）
- 没找到 → 创建新 group（**build**，也叫 miss）

**Selectivity** 控制的就是：100万行里，有多少行会"找到已有 group"，有多少行是"创建新 group"。

---

## 为什么不像副本1 那样直接随机

副本1 的做法：

```
定义 100 个 unique key
100万行随机选一个 key

结果: 大约第 1-100 行创建新 group, 后面 999900 行全是 hit
selectivity ≈ 100 / 1M = 0.0001 (固定的, 无法调)
```

问题：你没法精确控制"有多少行是 miss"。groups=100 时 selectivity 只能是 0.0001，想测 50% miss 怎么办？

---

## 副本2 的做法: 精确控制 hit/miss 比例

### Step 1: 生成 build keys (确定的 unique keys)

```
ht_size = 1024, load_factor = 0.5
num_keys = 1024 * 0.5 = 512 个 unique key

build_a = [1, 98, 195, 292, ..., 49665]   ← 512 个确定值
build_b = [7, 60, 113, 166, ..., 27079]   ← 512 个确定值
build_hashes = [xxh3(1,7), xxh3(98,60), ...]
```

### Step 2: 生成 probe keys (控制 hit/miss 比例)

```
selectivity = 0.7, num_probe_rows = 1M

num_hits = 1M * 0.7 = 700,000  (这些 key 在 build 里存在)
num_misses = 1M * 0.3 = 300,000  (这些 key 在 build 里不存在)
```

**Hit 数据**: 从 512 个 build keys 里随机选

```
for _ in 0..700000:
    idx = random(0..512)
    probe_a.push(build_a[idx])   ← 一定能在 hash table 里找到
    probe_b.push(build_b[idx])
```

**Miss 数据**: 构造保证不存在的 key

```
miss_base = 很大的数 (远离 build keys)
for i in 0..300000:
    probe_a.push(miss_base + i*31)   ← 一定不在 hash table 里
    probe_b.push(miss_base + i*17)
```

### Step 3: 合并 + 打乱

```
all_a = build_a ++ probe_a   (512 + 1M 行)
all_b = build_b ++ probe_b
all_hashes = build_hashes ++ probe_hashes

然后 shuffle probe 部分的顺序 (hit 和 miss 随机交错)
```

### Step 4: 跑 hash agg

```
对 all_a/all_b/all_hashes (共 1,000,512 行) 逐行做:
  查 hash table → 找到? 更新 sum : 插入新 group
```

---

## 具体例子

```
参数: ht_size=1024, lf=0.5, selectivity=0.7

数据:
  build 部分 (前 512 行): 全是新 key → 全 build
  probe 部分 (后 1M 行):
    700,000 行是已有 key → probe hit → 更新 sum
    300,000 行是新 key → probe miss → 创建新 group

hash agg 执行时:
  row 0-511: 创建 512 个 group (build)
  row 512-...: 混合 hit 和 miss
    有些行找到已有 group → sums[gid] += val
    有些行找不到 → 创建新 group → sums.push(val)
```

---

## selectivity 的含义

```
selectivity = probe_hits / probe_rows

sel=0.9: 90% 的 probe 行能找到已有 group (大量重复, 低基数, TPC-H Q1 风格)
sel=0.5: 50% hit, 50% miss (中等)
sel=0.1: 10% hit, 90% miss (几乎每行都是新 group, 高基数)
```

| selectivity | 含义 | hash table 压力 |
|-------------|------|----------------|
| 0.9 | 90% 行命中已有 group | comparator 被大量调用 |
| 0.5 | 一半 hit 一半 miss | 中等 |
| 0.1 | 10% hit, 大部分是新 group | comparator 很少调用, 大量 insert |

---

## 为什么这是 hash agg 不是 hash join

Hash join: 先建好一张表, 然后用另一张表去查 (probe 只读, 不插入)
Hash agg: 每行进来都可能插入新 group (probe + insert 混合)

副本2 的 `all_data` 合在一起逐行处理——每行要么 hit (更新 sum) 要么 miss (插入新 group)。这就是 hash agg 的行为。

虽然数据生成时分了 build/probe 两步, 但最终合并成一个流统一处理, 每行都走 "查找 → hit/miss → 更新/插入" 的完整 hash agg 流程。

---

## 对比两个副本

| | 副本1 | 副本2 |
|--|------|------|
| 数据生成 | 100万行随机选 100 个 key | 分 build+probe, 精确控制 hit/miss |
| Selectivity 控制 | 间接 (通过 num_groups) | 直接 (0.1~0.9) |
| Build/Probe 分离 | 否, 混在一起 | 是, 分开生成再合并 |
| 最终执行流程 | 逐行 find-or-insert | 逐行 find-or-insert (一样) |
| Hit/Miss 比例 | 约 (1 - groups/rows) | 精确 = selectivity 参数 |
