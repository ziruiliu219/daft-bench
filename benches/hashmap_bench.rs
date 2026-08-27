//! Hash table benchmark: TaperHashMap vs Daft-style hashbrown (staged aggregation)
//!
//! Data generation: generate_mixed_data (build + probe with selectivity)
//! Taper: persistent hash table, multi-batch (410 rows/batch)
//! Daft: staged aggregation (per-batch local agg → final merge)

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
// Data generation (your original structure)

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

    // Combine
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
// Taper runner (persistent hash table, multi-batch)

const BATCH_SIZE: usize = 410;

#[inline(never)]
fn run_taper_mixed(data: &MixedBenchData, num_chunks: usize) -> usize {
    let mut col_descs: Vec<ColumnDesc> = Vec::new();
    for _ in 0..data.num_str_cols { col_descs.push(ColumnDesc::Varchar); }
    for _ in 0..data.num_int_cols { col_descs.push(ColumnDesc::Int64); }

    let mut table = TaperColumnSerializeHandler::new(&col_descs, 8, num_chunks);
    let total_rows = data.hashes.len();
    let num_batches = (total_rows + BATCH_SIZE - 1) / BATCH_SIZE;

    for batch_idx in 0..num_batches {
        let start = batch_idx * BATCH_SIZE;
        let end = (start + BATCH_SIZE).min(total_rows);

        let batch_hashes = &data.hashes[start..end];
        let batch_values = &data.values[start..end];
        let str_slices: Vec<Vec<&[u8]>> = (0..data.num_str_cols)
            .map(|c| data.str_cols[c][start..end].iter().map(|s| s.as_slice()).collect())
            .collect();

        let mut columns: Vec<ColumnInput> = Vec::new();
        for c in 0..data.num_str_cols { columns.push(ColumnInput::Varchar(&str_slices[c])); }
        for c in 0..data.num_int_cols { columns.push(ColumnInput::Int64(&data.int_cols[c][start..end])); }

        table.emplace_table_with_decode(batch_hashes, &columns, batch_values);
    }
    black_box(table.num_groups());
    // Return final chunk count so callers can verify rehash occurred.
    table.map_num_chunks()
}

// ═══════════════════════════════════════════════════════════════════
// Daft runner: staged aggregation (per-batch local agg → final merge)
//
// Mirrors Daft's actual data structures:
// - Arrow-style columnar storage (offsets + data buffer for strings, Vec<i64> for ints)
// - RecordBatch = Vec of columns
// - comparator(i, j) compares two rows within the same columnar batch
// - take(indices) copies key data into new Arrow arrays (partial result)
// - concat merges all partial results into one big columnar batch
// - final agg uses same HashMap+comparator pattern on the concat'd batch

/// Arrow-style Utf8 column: offsets + contiguous data buffer.
/// Mirrors Arrow's Utf8Array<i64>.
struct Utf8Column {
    offsets: Vec<i64>,   // length = num_rows + 1
    data: Vec<u8>,       // all string bytes concatenated
}

impl Utf8Column {
    fn new() -> Self {
        Self { offsets: vec![0], data: Vec::new() }
    }

    fn with_capacity(num_rows: usize, avg_bytes: usize) -> Self {
        let mut offsets = Vec::with_capacity(num_rows + 1);
        offsets.push(0);
        Self { offsets, data: Vec::with_capacity(num_rows * avg_bytes) }
    }

    fn len(&self) -> usize { self.offsets.len() - 1 }

    /// Append a string value.
    fn push(&mut self, val: &[u8]) {
        self.data.extend_from_slice(val);
        self.offsets.push(self.data.len() as i64);
    }

    /// Get string at row index.
    #[inline(always)]
    fn get(&self, idx: usize) -> &[u8] {
        let start = self.offsets[idx] as usize;
        let end = self.offsets[idx + 1] as usize;
        &self.data[start..end]
    }

    /// take(indices) → new Utf8Column with copied data. Mirrors arrow::compute::take.
    fn take(&self, indices: &[u64]) -> Self {
        let mut result = Self::with_capacity(indices.len(), 16);
        for &idx in indices {
            result.push(self.get(idx as usize));
        }
        result
    }

    /// Concat multiple Utf8Columns into one. Mirrors Arrow concat.
    fn concat(columns: &[&Self]) -> Self {
        let total_rows: usize = columns.iter().map(|c| c.len()).sum();
        let total_bytes: usize = columns.iter().map(|c| c.data.len()).sum();
        let mut result = Self {
            offsets: Vec::with_capacity(total_rows + 1),
            data: Vec::with_capacity(total_bytes),
        };
        result.offsets.push(0);
        for col in columns {
            for i in 0..col.len() {
                result.push(col.get(i));
            }
        }
        result
    }
}

/// Arrow-style Int64 column (just a Vec<i64>). Mirrors Arrow's PrimitiveArray<Int64>.
struct Int64Column {
    values: Vec<i64>,
}

impl Int64Column {
    fn new() -> Self { Self { values: Vec::new() } }
    fn with_capacity(n: usize) -> Self { Self { values: Vec::with_capacity(n) } }
    fn len(&self) -> usize { self.values.len() }
    fn push(&mut self, val: i64) { self.values.push(val); }

    #[inline(always)]
    fn get(&self, idx: usize) -> i64 { self.values[idx] }

    fn take(&self, indices: &[u64]) -> Self {
        let mut result = Self::with_capacity(indices.len());
        for &idx in indices {
            result.values.push(self.values[idx as usize]);
        }
        result
    }

    fn concat(columns: &[&Self]) -> Self {
        let total: usize = columns.iter().map(|c| c.len()).sum();
        let mut result = Self::with_capacity(total);
        for col in columns {
            result.values.extend_from_slice(&col.values);
        }
        result
    }
}

/// A partial result from one batch. Mirrors Daft's MicroPartition/RecordBatch
/// that gets pushed into state.partially_aggregated.
/// Contains: group key columns (Arrow-style) + aggregated sum column.
struct PartialResult {
    str_key_cols: Vec<Utf8Column>,    // group key string columns
    int_key_cols: Vec<Int64Column>,   // group key int columns
    hashes: Vec<u64>,                 // hash of each group key
    sums: Vec<i64>,                   // partial sum for each group
}

#[inline(never)]
fn run_daft_staged(data: &MixedBenchData, capacity: usize) {
    let total_rows = data.hashes.len();
    let num_batches = (total_rows + BATCH_SIZE - 1) / BATCH_SIZE;

    // state.partially_aggregated: Vec<MicroPartition> in Daft
    let mut partial_results: Vec<PartialResult> = Vec::with_capacity(num_batches);

    for batch_idx in 0..num_batches {
        let start = batch_idx * BATCH_SIZE;
        let end = (start + BATCH_SIZE).min(total_rows);
        let batch_len = end - start;

        // ═══ Phase 1: agg_generic_hash_path ═══
        // Build per-batch HashMap. comparator compares two rows within this batch.
        // Daft real source: initial_capacity = min(num_rows, 1024).max(1)
        let init_cap = batch_len.min(1024).max(1);
        let mut table = HashMap::<IndexHash, u32, IdentityBuildHasher>::with_capacity_and_hasher(
            init_cap, Default::default(),
        );
        let mut groupkey_indices: Vec<u64> = Vec::new();
        let mut group_ids: Vec<u32> = Vec::with_capacity(batch_len);
        let mut num_groups: u32 = 0;

        for i in start..end {
            let h = data.hashes[i];
            let entry = table.raw_entry_mut().from_hash(h, |other| {
                if h != other.hash { return false; }
                let j = other.idx as usize;
                // comparator(i, j): compare two rows in the same batch
                for c in 0..data.num_str_cols {
                    if data.str_cols[c][i] != data.str_cols[c][j] { return false; }
                }
                for c in 0..data.num_int_cols {
                    if data.int_cols[c][i] != data.int_cols[c][j] { return false; }
                }
                true
            });
            let gid = match entry {
                RawEntryMut::Vacant(e) => {
                    let gid = num_groups;
                    num_groups += 1;
                    e.insert_hashed_nocheck(h, IndexHash { idx: i as u64, hash: h }, gid);
                    groupkey_indices.push(i as u64);
                    gid
                }
                RawEntryMut::Occupied(e) => *e.get(),
            };
            group_ids.push(gid);
        }

        // ═══ Phase 2: accumulate ═══
        // SumAccumulator: scatter values into per-group slots
        let mut sums: Vec<i64> = vec![0i64; num_groups as usize];
        for (row, &gid) in group_ids.iter().enumerate() {
            sums[gid as usize] += data.values[start + row];
        }

        // ═══ Phase 3: Construct partial result (take + finalize) ═══
        // groupby_table.take(&groupkey_indices) — copies key data into new Arrow arrays
        let mut partial_str_cols: Vec<Utf8Column> = Vec::with_capacity(data.num_str_cols);
        for c in 0..data.num_str_cols {
            let mut col = Utf8Column::with_capacity(num_groups as usize, 16);
            for &idx in &groupkey_indices {
                col.push(&data.str_cols[c][idx as usize]);  // ← string copy (arrow::compute::take)
            }
            partial_str_cols.push(col);
        }
        let mut partial_int_cols: Vec<Int64Column> = Vec::with_capacity(data.num_int_cols);
        for c in 0..data.num_int_cols {
            let mut col = Int64Column::with_capacity(num_groups as usize);
            for &idx in &groupkey_indices {
                col.push(data.int_cols[c][idx as usize]);
            }
            partial_int_cols.push(col);
        }

        // Hash column for the partial result (needed for final merge)
        let partial_hashes: Vec<u64> = groupkey_indices.iter()
            .map(|&idx| data.hashes[idx as usize])
            .collect();

        // Push to state.partially_aggregated
        partial_results.push(PartialResult {
            str_key_cols: partial_str_cols,
            int_key_cols: partial_int_cols,
            hashes: partial_hashes,
            sums,
        });
        // ← per-batch HashMap dropped here, batch data conceptually released
    }

    // ═══ Finalize: concat + final agg ═══

    // Step 1: MicroPartition::concat(partially_aggregated)
    // Concatenate all partial results into one big columnar batch
    let concat_str_cols: Vec<Utf8Column> = (0..data.num_str_cols).map(|c| {
        let refs: Vec<&Utf8Column> = partial_results.iter().map(|p| &p.str_key_cols[c]).collect();
        Utf8Column::concat(&refs)
    }).collect();
    let concat_int_cols: Vec<Int64Column> = (0..data.num_int_cols).map(|c| {
        let refs: Vec<&Int64Column> = partial_results.iter().map(|p| &p.int_key_cols[c]).collect();
        Int64Column::concat(&refs)
    }).collect();
    let concat_hashes: Vec<u64> = partial_results.iter().flat_map(|p| p.hashes.iter().copied()).collect();
    let concat_sums: Vec<i64> = partial_results.iter().flat_map(|p| p.sums.iter().copied()).collect();
    let concat_len = concat_hashes.len();

    // Step 2: concated.agg(final_agg_exprs, final_group_by)
    // Same pattern: HashMap + comparator(i, j) on the concat'd batch
    // Daft real source: initial_capacity = min(num_rows, 1024).max(1)
    let final_init_cap = concat_len.min(1024).max(1);
    let mut final_table = HashMap::<IndexHash, u32, IdentityBuildHasher>::with_capacity_and_hasher(
        final_init_cap, Default::default(),
    );
    let mut final_groupkey_indices: Vec<u64> = Vec::new();
    let mut final_group_ids: Vec<u32> = Vec::with_capacity(concat_len);
    let mut final_num_groups: u32 = 0;

    for i in 0..concat_len {
        let h = concat_hashes[i];
        let entry = final_table.raw_entry_mut().from_hash(h, |other| {
            if h != other.hash { return false; }
            let j = other.idx as usize;
            // comparator(i, j): compare two rows in the concat'd batch
            for c in 0..data.num_str_cols {
                if concat_str_cols[c].get(i) != concat_str_cols[c].get(j) { return false; }
            }
            for c in 0..data.num_int_cols {
                if concat_int_cols[c].get(i) != concat_int_cols[c].get(j) { return false; }
            }
            true
        });
        let gid = match entry {
            RawEntryMut::Vacant(e) => {
                let gid = final_num_groups;
                final_num_groups += 1;
                e.insert_hashed_nocheck(h, IndexHash { idx: i as u64, hash: h }, gid);
                final_groupkey_indices.push(i as u64);
                gid
            }
            RawEntryMut::Occupied(e) => *e.get(),
        };
        final_group_ids.push(gid);
    }

    // Final accumulate: SUM of partial SUMs
    let mut final_sums: Vec<i64> = vec![0i64; final_num_groups as usize];
    for (row, &gid) in final_group_ids.iter().enumerate() {
        final_sums[gid as usize] += concat_sums[row];
    }

    black_box(final_sums.len());
}

// ═══════════════════════════════════════════════════════════════════
// Hashbrown persistent runner: single hash table across all batches
// (same pattern as Taper — no per-batch rebuild)
//
// Key data is COPIED into a persistent arena on first insert.
// This is mandatory in real engines: once a batch is released, the
// hashmap comparator can't reference batch memory anymore.
// Mirrors Taper's RowContainer serialization.

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

                    // Daft hashbrown init capacity (real source: min(rows,1024).max(1), handled per-batch inside runner)
                    let num_misses = num_probe_rows - (num_probe_rows as f64 * selectivity) as usize;
                    let distinct_keys = num_keys + num_misses;
                    let daft_capacity = ((distinct_keys as f64 / 0.75) as usize).max(8);

                    // Taper: start from 1 chunk, matching OmniOperator Base::Init(0) (grows via 2x rehash)
                    let num_chunks = 1;

                    // Verify rehash actually happens: run once, check final chunk count grew past initial.
                    let final_chunks = run_taper_mixed(&data, num_chunks);
                    assert!(
                        final_chunks > num_chunks,
                        "REHASH NOT TRIGGERED for {}: started {} chunks, ended {} chunks (distinct_keys={})",
                        param, num_chunks, final_chunks, distinct_keys
                    );
                    eprintln!(
                        "[rehash-verify] {}: 1 -> {} chunks ({} rehashes, distinct_keys={})",
                        param, final_chunks, (final_chunks as f64).log2() as u32, distinct_keys
                    );

                    group.bench_with_input(BenchmarkId::new("taper", &param), &data, |b, d| {
                        b.iter(|| run_taper_mixed(black_box(d), num_chunks));
                    });
                    group.bench_with_input(BenchmarkId::new("daft_staged", &param), &data, |b, d| {
                        b.iter(|| run_daft_staged(black_box(d), daft_capacity));
                    });
                }
            }
        }
    }

    group.finish();
}

criterion_group!(benches, bench_hashagg);
criterion_main!(benches);
