//! Hash table benchmark: TaperHashMap vs Daft-style hashbrown
//!
//! Single-batch mode: all rows processed in ONE batch (no multi-batch, no staged agg)
//! HT pre-allocated large enough to guarantee NO rehash/expand during the run.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hashbrown::{HashMap, hash_map::RawEntryMut};
use rand_mt::Mt19937GenRand64;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use taper_hashmap::column_marshaller::{TaperColumnSerializeHandler, ColumnDesc, ColumnInput};
use xxhash_rust::xxh3::xxh3_64_with_seed;

// ═══════════════════════════════════════════════════════════════════
// Infra

#[derive(Default)]
struct IdentityHasher(u64);
impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 { self.0 }
    fn write(&mut self, _: &[u8]) { unreachable!() }
    fn write_u64(&mut self, v: u64) { self.0 = v; }
}
type IdentityBuildHasher = BuildHasherDefault<IdentityHasher>;

#[derive(Clone, Copy, Eq, PartialEq)]
struct IndexHash { idx: u64, hash: u64 }
impl Hash for IndexHash {
    fn hash<H: Hasher>(&self, state: &mut H) { state.write_u64(self.hash); }
}

fn hash_bytes(data: &[u8], seed: u64) -> u64 { xxh3_64_with_seed(data, seed) }
fn hash_combine(seed: u64, val: i64) -> u64 { xxh3_64_with_seed(&val.to_le_bytes(), seed) }
fn gen_string(prefix: &str, i: usize, c: usize) -> Vec<u8> {
    format!("{}_{}_c{}", prefix, i, c).into_bytes()
}

// ═══════════════════════════════════════════════════════════════════
// Data generation

struct MixedBenchData {
    str_cols: Vec<Vec<Vec<u8>>>,
    int_cols: Vec<Vec<i64>>,
    hashes: Vec<u64>,
    values: Vec<i64>,
    num_str_cols: usize,
    num_int_cols: usize,
}

fn generate_mixed_data(
    num_str_cols: usize, num_int_cols: usize,
    num_keys: usize, num_probe_rows: usize, selectivity: f64,
    rng: &mut Mt19937GenRand64,
) -> MixedBenchData {
    // Build keys
    let mut build_str_cols: Vec<Vec<Vec<u8>>> = (0..num_str_cols)
        .map(|c| (0..num_keys).map(|i| gen_string("key", i, c)).collect()).collect();
    let build_int_cols: Vec<Vec<i64>> = (0..num_int_cols)
        .map(|c| (0..num_keys).map(|i| i as i64 * (97 + c as i64 * 31) + 1).collect()).collect();
    let build_hashes: Vec<u64> = (0..num_keys).map(|i| {
        let mut h = 0u64;
        for c in 0..num_str_cols { h = hash_bytes(&build_str_cols[c][i], h); }
        for c in 0..num_int_cols { h = hash_combine(h, build_int_cols[c][i]); }
        h
    }).collect();
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
    let probe_str_cols: Vec<Vec<Vec<u8>>> = (0..num_str_cols)
        .map(|c| order.iter().map(|&i| probe_str_cols[c][i].clone()).collect()).collect();
    let probe_int_cols: Vec<Vec<i64>> = (0..num_int_cols)
        .map(|c| order.iter().map(|&i| probe_int_cols[c][i]).collect()).collect();
    let probe_hashes: Vec<u64> = order.iter().map(|&i| probe_hashes[i]).collect();
    let probe_values: Vec<i64> = (0..num_probe_rows).map(|i| (i % 1000) as i64).collect();

    // Combine build + probe into one array
    for c in 0..num_str_cols { build_str_cols[c].extend(probe_str_cols[c].clone()); }
    let mut all_int_cols = build_int_cols;
    for c in 0..num_int_cols { all_int_cols[c].extend_from_slice(&probe_int_cols[c]); }
    let mut all_hashes = build_hashes; all_hashes.extend_from_slice(&probe_hashes);
    let mut all_values = build_values; all_values.extend_from_slice(&probe_values);

    MixedBenchData {
        str_cols: build_str_cols, int_cols: all_int_cols,
        hashes: all_hashes, values: all_values,
        num_str_cols, num_int_cols,
    }
}

// ═══════════════════════════════════════════════════════════════════
// Taper runner: single batch, all rows at once

#[inline(never)]
fn run_taper_mixed(data: &MixedBenchData, num_chunks: usize) {
    let mut col_descs: Vec<ColumnDesc> = Vec::new();
    for _ in 0..data.num_str_cols { col_descs.push(ColumnDesc::Varchar); }
    for _ in 0..data.num_int_cols { col_descs.push(ColumnDesc::Int64); }

    let mut table = TaperColumnSerializeHandler::new(&col_descs, 8, num_chunks);

    // Build column inputs for ALL rows at once
    let str_slices: Vec<Vec<&[u8]>> = (0..data.num_str_cols)
        .map(|c| data.str_cols[c].iter().map(|s| s.as_slice()).collect())
        .collect();

    let mut columns: Vec<ColumnInput> = Vec::new();
    for c in 0..data.num_str_cols { columns.push(ColumnInput::Varchar(&str_slices[c])); }
    for c in 0..data.num_int_cols { columns.push(ColumnInput::Int64(&data.int_cols[c])); }

    // Single batch: process all rows in one call
    table.emplace_table_with_decode(&data.hashes, &columns, &data.values);
    black_box(table.num_groups());
}

// ═══════════════════════════════════════════════════════════════════
// Daft runner: single batch, all rows at once (pre-allocated, no rehash)

#[inline(never)]
fn run_daft_mixed(data: &MixedBenchData, capacity: usize) {
    let mut table = HashMap::<IndexHash, u32, IdentityBuildHasher>::with_capacity_and_hasher(
        capacity, Default::default(),
    );
    let mut ngroups: u32 = 0;
    let mut sums = Vec::<i64>::with_capacity(capacity);

    for (i, &h) in data.hashes.iter().enumerate() {
        let entry = table.raw_entry_mut().from_hash(h, |other| {
            if h != other.hash { return false; }
            let oi = other.idx as usize;
            for c in 0..data.num_str_cols {
                if data.str_cols[c][i] != data.str_cols[c][oi] { return false; }
            }
            for c in 0..data.num_int_cols {
                if data.int_cols[c][i] != data.int_cols[c][oi] { return false; }
            }
            true
        });
        match entry {
            RawEntryMut::Occupied(e) => { sums[*e.get() as usize] += data.values[i]; }
            RawEntryMut::Vacant(e) => {
                e.insert_hashed_nocheck(h, IndexHash { idx: i as u64, hash: h }, ngroups);
                ngroups += 1;
                sums.push(data.values[i]);
            }
        }
    }
    black_box(&sums);
}

// ═══════════════════════════════════════════════════════════════════
// Benchmark registration

fn bench_hashagg(c: &mut Criterion) {
    let mut group = c.benchmark_group("hashagg");
    group.sample_size(20);
    let num_probe_rows: usize = 1_000_000;

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
                    let param = format!("{}_ht={}_lf={:.2}_sel={:.1}", type_name, ht_size, load_factor, selectivity);

                    // Compute distinct keys to pre-allocate (no rehash)
                    let num_misses = num_probe_rows - (num_probe_rows as f64 * selectivity) as usize;
                    let distinct_keys = num_keys + num_misses;

                    // Taper: allocate enough chunks so load < 0.9 (LOAD_FACTOR_THRESHOLD)
                    // Each chunk has 8 slots. We want: distinct_keys / (num_chunks * 8) < 0.9
                    let min_slots = ((distinct_keys as f64 / 0.85) as usize).max(8);
                    let num_chunks = ((min_slots + 7) / 8).next_power_of_two();

                    // Daft (hashbrown): default load factor ~0.875, give extra room
                    let daft_capacity = ((distinct_keys as f64 / 0.7) as usize).max(8);

                    group.bench_with_input(BenchmarkId::new("taper", &param), &data, |b, d| {
                        b.iter(|| run_taper_mixed(black_box(d), num_chunks));
                    });
                    group.bench_with_input(BenchmarkId::new("daft", &param), &data, |b, d| {
                        b.iter(|| run_daft_mixed(black_box(d), daft_capacity));
                    });
                }
            }
        }
    }

    group.finish();
}

criterion_group!(benches, bench_hashagg);
criterion_main!(benches);
