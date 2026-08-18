//! Hash table microbenchmark: TaperHashMap vs hashbrown (Daft-style)
//!
//! 纯 hash table 层面的性能测试
//!
//! Key Types:
//!   - 1col_i32:  单列 i32 key (4B)
//!   - 1col_i64:  单列 i64 key (8B)
//!   - 2col_i64:  两列 i64 key (16B)
//!   - 4col_i64:  四列 i64 key (32B)
//!   - 4str_0int: 4 varchar + 0 int key
//!   - 3str_1int: 3 varchar + 1 int key
//!   - 2str_2int: 2 varchar + 2 int key
//!   - 1str_3int: 1 varchar + 3 int key
//!
//! 参数:
//!   - HT size: 256, 1024, 4096, 16384
//!   - Load Factor: 0.5, 0.75
//!   - Selectivity: 0.1 ~ 0.9

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hashbrown::{HashMap, hash_map::RawEntryMut};
use rand_mt::Mt19937GenRand64;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use taper_hashmap::column_marshaller::{TaperColumnSerializeHandler, ColumnDesc, ColumnInput};
use xxhash_rust::xxh3::xxh3_64_with_seed;
use arrow::array::{ArrayRef, Array, Int64Array, StringArray};
use std::sync::Arc;

// ═══════════════════════════════════════════════════════════════════
// Daft infra
// ═══════════════════════════════════════════════════════════════════

#[derive(Default)]
struct IdentityHasher(u64);
impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 { self.0 }
    fn write(&mut self, _: &[u8]) { unreachable!() }
    fn write_u64(&mut self, i: u64) { self.0 = i; }
}
type IdentityBuildHasher = BuildHasherDefault<IdentityHasher>;

#[derive(Eq, PartialEq)]
struct IndexHash { idx: u64, hash: u64 }
impl Hash for IndexHash {
    fn hash<H: Hasher>(&self, state: &mut H) { state.write_u64(self.hash); }
}

// ═══════════════════════════════════════════════════════════════════
// Hash functions
// ═══════════════════════════════════════════════════════════════════

#[inline]
fn hash_combine(seed: u64, val: i64) -> u64 {
    xxh3_64_with_seed(&val.to_le_bytes(), seed)
}

#[inline]
fn hash_bytes(data: &[u8], seed: u64) -> u64 {
    xxh3_64_with_seed(data, seed)
}

// ═══════════════════════════════════════════════════════════════════
// Mixed (varchar + int) key benchmark data
// ═══════════════════════════════════════════════════════════════════

struct MixedBenchData {
    /// String columns data (each element is a Vec of byte strings)
    str_cols: Vec<Vec<Vec<u8>>>,
    /// Int columns data
    int_cols: Vec<Vec<i64>>,
    hashes: Vec<u64>,
    values: Vec<i64>,
    num_str_cols: usize,
    num_int_cols: usize,
}

/// Generate random string of given average length (8~13 bytes)
fn gen_string( base: &str, id: usize, col: usize) -> Vec<u8> {
    format!("{}_{}_c{}", base, id, col).into_bytes()
}

fn generate_mixed_data(
    num_str_cols: usize,
    num_int_cols: usize,
    num_keys: usize,
    num_probe_rows: usize,
    selectivity: f64,
    rng: &mut Mt19937GenRand64,
) -> MixedBenchData {
    // Build keys
    let mut build_str_cols: Vec<Vec<Vec<u8>>> = (0..num_str_cols)
        .map(|c| (0..num_keys).map(|i| gen_string("key", i, c)).collect())
        .collect();
    let mut build_int_cols: Vec<Vec<i64>> = (0..num_int_cols)
        .map(|c| (0..num_keys).map(|i| i as i64 * (97 + c as i64 * 31) + 1).collect())
        .collect();

    // Build hashes
    let build_hashes: Vec<u64> = (0..num_keys)
        .map(|i| {
            let mut h = 0u64;
            for c in 0..num_str_cols {
                h = hash_bytes(&build_str_cols[c][i], h);
            }
            for c in 0..num_int_cols {
                h = hash_combine(h, build_int_cols[c][i]);
            }
            h
        })
        .collect();

    let build_values: Vec<i64> = (0..num_keys).map(|i| (i % 1000) as i64).collect();

    // Probe keys
    let num_hits = (num_probe_rows as f64 * selectivity) as usize;
    let num_misses = num_probe_rows - num_hits;

    let mut probe_str_cols: Vec<Vec<Vec<u8>>> = vec![Vec::with_capacity(num_probe_rows); num_str_cols];
    let mut probe_int_cols: Vec<Vec<i64>> = vec![Vec::with_capacity(num_probe_rows); num_int_cols];
    let mut probe_hashes: Vec<u64> = Vec::with_capacity(num_probe_rows);

    for _ in 0..num_hits {
        let idx = (rng.next_u64() as usize) % num_keys;
        for c in 0..num_str_cols { probe_str_cols[c].push(build_str_cols[c][idx].clone()); }
        for c in 0..num_int_cols { probe_int_cols[c].push(build_int_cols[c][idx]); }
        probe_hashes.push(build_hashes[idx]);
    }

    for i in 0..num_misses {
        let mut h = 0u64;
        for c in 0..num_str_cols {
            let s = format!("miss_{}_{}", i, c).into_bytes();
            h = hash_bytes(&s, h);
            probe_str_cols[c].push(s);
        }
        for c in 0..num_int_cols {
            let v = (num_keys as i64 + 1) * 200 + i as i64 * 31 + c as i64;
            h = hash_combine(h, v);
            probe_int_cols[c].push(v);
        }
        probe_hashes.push(h);
    }

    // Shuffle probe
    let mut order: Vec<usize> = (0..num_probe_rows).collect();
    for i in (1..num_probe_rows).rev() { order.swap(i, (rng.next_u64() as usize) % (i + 1)); }
    let mut probe_str_cols: Vec<Vec<Vec<u8>>> = (0..num_str_cols).map(|c| order.iter().map(|&i| probe_str_cols[c][i].clone()).collect()).collect();
    let probe_int_cols: Vec<Vec<i64>> = (0..num_int_cols).map(|c| order.iter().map(|&i| probe_int_cols[c][i]).collect()).collect();
    let probe_hashes: Vec<u64> = order.iter().map(|&i| probe_hashes[i]).collect();
    let probe_values: Vec<i64> = (0..num_probe_rows).map(|i| (i % 1000) as i64).collect();

    // Combine build + probe
    for c in 0..num_str_cols {
        let drain: Vec<Vec<u8>> = std::mem::take(&mut probe_str_cols[c]);
        build_str_cols[c].extend(drain);
    }
    for c in 0..num_int_cols { build_int_cols[c].extend_from_slice(&probe_int_cols[c]); }
    let mut all_hashes = build_hashes; all_hashes.extend_from_slice(&probe_hashes);
    let mut all_values = build_values; all_values.extend_from_slice(&probe_values);

    MixedBenchData {
        str_cols: build_str_cols,
        int_cols: build_int_cols,
        hashes: all_hashes,
        values: all_values,
        num_str_cols,
        num_int_cols,
    }
}

// ═══════════════════════════════════════════════════════════════════
// Taper runners
// ═══════════════════════════════════════════════════════════════════

#[inline(never)]
fn run_taper_mixed(data: &MixedBenchData, num_chunks: usize) {
    const BATCH_SIZE: usize = 410;

    // Build schema
    let mut col_descs: Vec<ColumnDesc> = Vec::new();
    for _ in 0..data.num_str_cols { col_descs.push(ColumnDesc::Varchar); }
    for _ in 0..data.num_int_cols { col_descs.push(ColumnDesc::Int64); }

    let mut table = TaperColumnSerializeHandler::new(&col_descs, 8, num_chunks);

    let total_rows = data.hashes.len();
    let num_batches = (total_rows + BATCH_SIZE - 1) / BATCH_SIZE;

    for batch_idx in 0..num_batches {
        let start = batch_idx * BATCH_SIZE;
        let end = (start + BATCH_SIZE).min(total_rows);
        let batch_len = end - start;

        // Slice this batch's data
        let batch_hashes = &data.hashes[start..end];
        let batch_values = &data.values[start..end];

        let str_slices: Vec<Vec<&[u8]>> = (0..data.num_str_cols)
            .map(|c| data.str_cols[c][start..end].iter().map(|s| s.as_slice()).collect())
            .collect();

        let mut columns: Vec<ColumnInput> = Vec::new();
        for c in 0..data.num_str_cols {
            columns.push(ColumnInput::Varchar(&str_slices[c]));
        }
        for c in 0..data.num_int_cols {
            columns.push(ColumnInput::Int64(&data.int_cols[c][start..end]));
        }

        table.emplace_table_with_decode(batch_hashes, &columns, batch_values);
    }

    black_box(table.num_groups());
}

// ═══════════════════════════════════════════════════════════════════
// Daft runners — uses real Arrow arrays to match Daft's actual code path
// ═══════════════════════════════════════════════════════════════════

/// Build a multi-column equality comparator from Arrow arrays.
/// This mirrors Daft's `build_multi_array_is_equal` in comparison.rs:
///   - Box<dyn Fn(usize, usize) -> bool> outer closure
///   - Per-column: downcast ArrayRef → typed access with offset indirection
///   - Validity bitmap check for nulls
fn build_multi_array_is_equal(arrays: &[ArrayRef]) -> Box<dyn Fn(usize, usize) -> bool + '_> {
    // Build per-column comparators as closures (matches Daft's pattern)
    let col_comparators: Vec<Box<dyn Fn(usize, usize) -> bool + '_>> = arrays.iter().map(|arr| {
        let arr_ref: &dyn arrow::array::Array = arr.as_ref();
        if let Some(str_arr) = arr_ref.as_any().downcast_ref::<StringArray>() {
            // String column: validity check + offset-based value access
            let comparator: Box<dyn Fn(usize, usize) -> bool + '_> = Box::new(move |i: usize, j: usize| {
                let i_null = str_arr.is_null(i);
                let j_null = str_arr.is_null(j);
                if i_null && j_null { return true; }
                if i_null || j_null { return false; }
                // value() does offset lookup: offsets[i]..offsets[i+1] into data buffer
                str_arr.value(i) == str_arr.value(j)
            });
            comparator
        } else if let Some(int_arr) = arr_ref.as_any().downcast_ref::<Int64Array>() {
            // Int64 column: validity check + direct value access
            let comparator: Box<dyn Fn(usize, usize) -> bool + '_> = Box::new(move |i: usize, j: usize| {
                let i_null = int_arr.is_null(i);
                let j_null = int_arr.is_null(j);
                if i_null && j_null { return true; }
                if i_null || j_null { return false; }
                int_arr.value(i) == int_arr.value(j)
            });
            comparator
        } else {
            panic!("unsupported array type");
        }
    }).collect();

    Box::new(move |i: usize, j: usize| -> bool {
        for cmp in col_comparators.iter() {
            if !cmp(i, j) { return false; }
        }
        true
    })
}

/// Convert MixedBenchData into Arrow arrays (done outside timing loop).
fn build_arrow_arrays(data: &MixedBenchData) -> Vec<ArrayRef> {
    let mut arrays: Vec<ArrayRef> = Vec::new();
    for c in 0..data.num_str_cols {
        let arr = StringArray::from_iter_values(
            data.str_cols[c].iter().map(|s| unsafe { std::str::from_utf8_unchecked(s) })
        );
        arrays.push(Arc::new(arr) as ArrayRef);
    }
    for c in 0..data.num_int_cols {
        let arr = Int64Array::from(data.int_cols[c].clone());
        arrays.push(Arc::new(arr) as ArrayRef);
    }
    arrays
}

#[inline(never)]
fn run_daft_mixed(data: &MixedBenchData, ht_size: usize, arrays: &[ArrayRef]) {
    const BATCH_SIZE: usize = 410;

    let mut table = HashMap::<IndexHash, u32, IdentityBuildHasher>::with_capacity_and_hasher(ht_size, Default::default());
    let mut ngroups: u32 = 0;
    let mut sums = Vec::<i64>::with_capacity(ht_size);

    // Build comparator from Arrow arrays — same as Daft's build_multi_array_is_equal
    let comparator = build_multi_array_is_equal(arrays);

    let total_rows = data.hashes.len();
    let num_batches = (total_rows + BATCH_SIZE - 1) / BATCH_SIZE;

    for batch_idx in 0..num_batches {
        let start = batch_idx * BATCH_SIZE;
        let end = (start + BATCH_SIZE).min(total_rows);

        for i in start..end {
            let h = data.hashes[i];
            let entry: RawEntryMut<'_, IndexHash, u32, BuildHasherDefault<IdentityHasher>> = table.raw_entry_mut().from_hash(h, |other| {
                if h != other.hash { return false; }
                comparator(i, other.idx as usize)
            });
            match entry {
                RawEntryMut::Occupied(e) => { sums[*e.get() as usize] += data.values[i]; }
                RawEntryMut::Vacant(e) => {
                    e.insert_hashed_nocheck(h, IndexHash { idx: i as u64, hash: h }, ngroups);
                    ngroups += 1; sums.push(data.values[i]);
                }
            }
        }
    }
    black_box(&sums);
}

// ═══════════════════════════════════════════════════════════════════
// Benchmark
// ═══════════════════════════════════════════════════════════════════

fn bench_hashagg(c: &mut Criterion) {
    let mut group = c.benchmark_group("hashagg");
    group.sample_size(20);
    let num_probe_rows = 1_000_000;

    // ─── Mixed (varchar + int) key types ───
    // (name, num_str_cols, num_int_cols)
    let mixed_key_types: &[(&str, usize, usize)] = &[
        ("4str_0int", 4, 0),
        ("3str_1int", 3, 1),
        ("2str_2int", 2, 2),
        ("1str_3int", 1, 3),
        ("0str_4int", 0, 4),
    ];

    for &(type_name, num_str, num_int) in mixed_key_types {
        for &ht_size in &[16384, 65536, 262144, 1048576] {
            for &load_factor in &[0.5, 0.75] {
                let num_keys = (ht_size as f64 * load_factor) as usize;
                for &selectivity in &[0.1, 0.3, 0.5, 0.7, 0.9] {
                    let mut rng = Mt19937GenRand64::new(42);
                    let data = generate_mixed_data(num_str, num_int, num_keys, num_probe_rows, selectivity, &mut rng);
                    let arrays = build_arrow_arrays(&data);
                    let param = format!("{}_ht={}_lf={:.2}_sel={:.1}", type_name, ht_size, load_factor, selectivity);

                    // Pre-allocate enough chunks so load stays below 0.9 threshold (no rehash).
                    // distinct_keys = num_keys + num_misses
                    let num_misses = num_probe_rows - (num_probe_rows as f64 * selectivity) as usize;
                    let distinct_keys = num_keys + num_misses;
                    let min_slots = ((distinct_keys as f64 / 0.85) as usize).max(8);
                    let num_chunks = ((min_slots + 7) / 8).next_power_of_two();

                    let daft_capacity = ((distinct_keys as f64 / 0.75) as usize).max(8);

                    group.bench_with_input(BenchmarkId::new("daft", &param), &data, |b, d| { b.iter(|| run_daft_mixed(black_box(d), daft_capacity, &arrays)); });
                    group.bench_with_input(BenchmarkId::new("taper", &param), &data, |b, d| { b.iter(|| run_taper_mixed(black_box(d), num_chunks)); });
                }
            }
        }
    }

    group.finish();
}

criterion_group!(benches, bench_hashagg);
criterion_main!(benches);
