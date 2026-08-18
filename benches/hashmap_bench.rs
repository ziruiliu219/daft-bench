use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use hashbrown::{hash_map::RawEntryMut, HashMap};
use rand_mt::Mt19937GenRand64;
use std::hash::{BuildHasherDefault, Hash, Hasher};
use taper_hashmap::column_marshaller::{ColumnDesc, ColumnInput, TaperColumnSerializeHandler};
use xxhash_rust::xxh3::xxh3_64_with_seed;

#[derive(Default)]
struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _: &[u8]) {
        unreachable!("IdentityHasher only accepts precomputed u64 hashes")
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

type IdentityBuildHasher = BuildHasherDefault<IdentityHasher>;

#[derive(Clone, Copy, Eq, PartialEq)]
struct IndexHash {
    idx: u64,
    hash: u64,
}

impl Hash for IndexHash {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

#[derive(Clone)]
struct Batch {
    str_cols: Vec<Vec<Vec<u8>>>,
    int_cols: Vec<Vec<i64>>,
    values: Vec<i64>,
}

impl Batch {
    fn len(&self) -> usize {
        self.values.len()
    }
}

#[derive(Clone)]
struct PartialRows {
    str_cols: Vec<Vec<Vec<u8>>>,
    int_cols: Vec<Vec<i64>>,
    sums: Vec<i64>,
}

impl PartialRows {
    fn empty(num_str_cols: usize, num_int_cols: usize) -> Self {
        Self {
            str_cols: vec![Vec::new(); num_str_cols],
            int_cols: vec![Vec::new(); num_int_cols],
            sums: Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.sums.len()
    }

    fn append_from_batch(&mut self, batch: &Batch, row_idx: usize, sum: i64) {
        for c in 0..self.str_cols.len() {
            self.str_cols[c].push(batch.str_cols[c][row_idx].clone());
        }
        for c in 0..self.int_cols.len() {
            self.int_cols[c].push(batch.int_cols[c][row_idx]);
        }
        self.sums.push(sum);
    }

    fn append_from_partial(&mut self, partial: &PartialRows, row_idx: usize, sum: i64) {
        for c in 0..self.str_cols.len() {
            self.str_cols[c].push(partial.str_cols[c][row_idx].clone());
        }
        for c in 0..self.int_cols.len() {
            self.int_cols[c].push(partial.int_cols[c][row_idx]);
        }
        self.sums.push(sum);
    }

    fn extend(&mut self, other: PartialRows) {
        for (dst, src) in self.str_cols.iter_mut().zip(other.str_cols) {
            dst.extend(src);
        }
        for (dst, src) in self.int_cols.iter_mut().zip(other.int_cols) {
            dst.extend(src);
        }
        self.sums.extend(other.sums);
    }
}

struct Workload {
    batches: Vec<Batch>,
    num_str_cols: usize,
    num_int_cols: usize,
    expected_groups: usize,
}

#[inline]
fn hash_bytes(seed: u64, bytes: &[u8]) -> u64 {
    xxh3_64_with_seed(bytes, seed)
}

#[inline]
fn hash_i64(seed: u64, value: i64) -> u64 {
    xxh3_64_with_seed(&value.to_le_bytes(), seed)
}

#[inline]
fn hash_u32(seed: u64, value: u32) -> u64 {
    xxh3_64_with_seed(&value.to_le_bytes(), seed)
}

fn hash_batch_row(batch: &Batch, row_idx: usize) -> u64 {
    let mut h = 0;
    for col in &batch.str_cols {
        h = hash_bytes(h, &col[row_idx]);
    }
    for col in &batch.int_cols {
        h = hash_i64(h, col[row_idx]);
    }
    h
}

fn hash_partial_row(rows: &PartialRows, row_idx: usize) -> u64 {
    let mut h = 0;
    for col in &rows.str_cols {
        h = hash_bytes(h, &col[row_idx]);
    }
    for col in &rows.int_cols {
        h = hash_i64(h, col[row_idx]);
    }
    h
}

fn batch_rows_equal(batch: &Batch, left: usize, right: usize) -> bool {
    for col in &batch.str_cols {
        if col[left] != col[right] { return false; }
    }
    for col in &batch.int_cols {
        if col[left] != col[right] { return false; }
    }
    true
}

fn partial_rows_equal(rows: &PartialRows, left: usize, right: usize) -> bool {
    for col in &rows.str_cols {
        if col[left] != col[right] { return false; }
    }
    for col in &rows.int_cols {
        if col[left] != col[right] { return false; }
    }
    true
}

fn make_key(id: usize, col: usize, len: usize) -> Vec<u8> {
    let mut s = format!("key_{id:012}_col_{col}_");
    while s.len() < len {
        s.push(char::from(b'a' + ((id + col + s.len()) % 26) as u8));
    }
    s.into_bytes()
}

fn generate_workload(
    num_batches: usize,
    batch_size: usize,
    num_str_cols: usize,
    num_int_cols: usize,
    global_cardinality: usize,
    new_key_rate: f64,
    string_len: usize,
    rng: &mut Mt19937GenRand64,
) -> Workload {
    let mut seen_ids: Vec<usize> = Vec::new();
    let mut next_id = 0usize;
    let mut batches = Vec::with_capacity(num_batches);

    for _ in 0..num_batches {
        let mut batch = Batch {
            str_cols: vec![Vec::with_capacity(batch_size); num_str_cols],
            int_cols: vec![Vec::with_capacity(batch_size); num_int_cols],
            values: Vec::with_capacity(batch_size),
        };

        for row in 0..batch_size {
            let draw = (rng.next_u64() as f64) / (u64::MAX as f64);
            let use_new = seen_ids.is_empty()
                || (next_id < global_cardinality && draw < new_key_rate);
            let key_id = if use_new {
                let id = next_id;
                next_id += 1;
                seen_ids.push(id);
                id
            } else {
                let idx = (rng.next_u64() as usize) % seen_ids.len();
                seen_ids[idx]
            };

            for c in 0..num_str_cols {
                batch.str_cols[c].push(make_key(key_id, c, string_len));
            }
            for c in 0..num_int_cols {
                batch.int_cols[c].push((key_id as i64 * 131) + (c as i64 * 17) + 7);
            }
            batch.values.push(((row + key_id) % 1000) as i64);
        }

        batches.push(batch);
    }

    Workload {
        batches,
        num_str_cols,
        num_int_cols,
        expected_groups: next_id.max(1),
    }
}

fn aggregate_batch_generic(batch: &Batch) -> PartialRows {
    let len = batch.len();
    let mut hashes = Vec::with_capacity(len);
    for row_idx in 0..len {
        hashes.push(hash_batch_row(batch, row_idx));
    }

    let mut table = HashMap::<IndexHash, u32, IdentityBuildHasher>::with_capacity_and_hasher(
        len.min(1024).max(1), Default::default(),
    );
    let mut reps = Vec::<usize>::new();
    let mut sums = Vec::<i64>::new();

    for (row_idx, &h) in hashes.iter().enumerate() {
        let entry = table.raw_entry_mut().from_hash(h, |other| {
            h == other.hash && batch_rows_equal(batch, row_idx, other.idx as usize)
        });
        match entry {
            RawEntryMut::Vacant(e) => {
                let gid = reps.len() as u32;
                e.insert_hashed_nocheck(h, IndexHash { idx: row_idx as u64, hash: h }, gid);
                reps.push(row_idx);
                sums.push(batch.values[row_idx]);
            }
            RawEntryMut::Occupied(e) => { sums[*e.get() as usize] += batch.values[row_idx]; }
        }
    }

    let mut out = PartialRows::empty(batch.str_cols.len(), batch.int_cols.len());
    for (gid, &row_idx) in reps.iter().enumerate() {
        out.append_from_batch(batch, row_idx, sums[gid]);
    }
    out
}

fn aggregate_partial_generic(rows: &PartialRows) -> PartialRows {
    let len = rows.len();
    let mut hashes = Vec::with_capacity(len);
    for row_idx in 0..len { hashes.push(hash_partial_row(rows, row_idx)); }

    let mut table = HashMap::<IndexHash, u32, IdentityBuildHasher>::with_capacity_and_hasher(
        len.min(1024).max(1), Default::default(),
    );
    let mut reps = Vec::<usize>::new();
    let mut sums = Vec::<i64>::new();

    for (row_idx, &h) in hashes.iter().enumerate() {
        let entry = table.raw_entry_mut().from_hash(h, |other| {
            h == other.hash && partial_rows_equal(rows, row_idx, other.idx as usize)
        });
        match entry {
            RawEntryMut::Vacant(e) => {
                let gid = reps.len() as u32;
                e.insert_hashed_nocheck(h, IndexHash { idx: row_idx as u64, hash: h }, gid);
                reps.push(row_idx);
                sums.push(rows.sums[row_idx]);
            }
            RawEntryMut::Occupied(e) => { sums[*e.get() as usize] += rows.sums[row_idx]; }
        }
    }

    let mut out = PartialRows::empty(rows.str_cols.len(), rows.int_cols.len());
    for (gid, &row_idx) in reps.iter().enumerate() {
        out.append_from_partial(rows, row_idx, sums[gid]);
    }
    out
}

fn symbolize_cols(str_cols: &[Vec<Vec<u8>>]) -> Vec<Vec<u32>> {
    let len = str_cols.first().map_or(0, Vec::len);
    let mut out = Vec::with_capacity(str_cols.len());
    for col in str_cols {
        let mut map = HashMap::<&[u8], u32>::with_capacity_and_hasher(len.min(1024).max(1), Default::default());
        let mut next = 0u32;
        let mut syms = Vec::with_capacity(len);
        for value in col {
            let id = match map.entry(value.as_slice()) {
                hashbrown::hash_map::Entry::Vacant(e) => { let id = next; next += 1; e.insert(id); id }
                hashbrown::hash_map::Entry::Occupied(e) => *e.get(),
            };
            syms.push(id);
        }
        out.push(syms);
    }
    out
}

fn aggregate_symbol_rows(symbols: &[Vec<u32>], int_cols: &[Vec<i64>], values: &[i64]) -> (Vec<usize>, Vec<i64>) {
    let len = values.len();
    let mut table = HashMap::<IndexHash, u32, IdentityBuildHasher>::with_capacity_and_hasher(len.min(1024).max(1), Default::default());
    let mut reps = Vec::<usize>::new();
    let mut sums = Vec::<i64>::new();

    for row_idx in 0..len {
        let mut h = 0;
        for col in symbols { h = hash_u32(h, col[row_idx]); }
        for col in int_cols { h = hash_i64(h, col[row_idx]); }
        let entry = table.raw_entry_mut().from_hash(h, |other| {
            if h != other.hash { return false; }
            let other_idx = other.idx as usize;
            for col in symbols { if col[row_idx] != col[other_idx] { return false; } }
            for col in int_cols { if col[row_idx] != col[other_idx] { return false; } }
            true
        });
        match entry {
            RawEntryMut::Vacant(e) => {
                let gid = reps.len() as u32;
                e.insert_hashed_nocheck(h, IndexHash { idx: row_idx as u64, hash: h }, gid);
                reps.push(row_idx); sums.push(values[row_idx]);
            }
            RawEntryMut::Occupied(e) => sums[*e.get() as usize] += values[row_idx],
        }
    }
    (reps, sums)
}

fn aggregate_packed_two_symbols(symbols: &[Vec<u32>], values: &[i64]) -> (Vec<usize>, Vec<i64>) {
    let len = values.len();
    let mut table = HashMap::<u64, u32>::with_capacity_and_hasher(len.min(1024).max(1), Default::default());
    let mut reps = Vec::<usize>::new();
    let mut sums = Vec::<i64>::new();

    for row_idx in 0..len {
        let packed = ((symbols[0][row_idx] as u64) << 32) | symbols[1][row_idx] as u64;
        match table.entry(packed) {
            hashbrown::hash_map::Entry::Vacant(e) => {
                let gid = reps.len() as u32; e.insert(gid);
                reps.push(row_idx); sums.push(values[row_idx]);
            }
            hashbrown::hash_map::Entry::Occupied(e) => sums[*e.get() as usize] += values[row_idx],
        }
    }
    (reps, sums)
}

fn aggregate_batch_symbolized(batch: &Batch) -> PartialRows {
    let symbols = symbolize_cols(&batch.str_cols);
    let (reps, sums) = if batch.str_cols.len() == 2 && batch.int_cols.is_empty() {
        aggregate_packed_two_symbols(&symbols, &batch.values)
    } else {
        aggregate_symbol_rows(&symbols, &batch.int_cols, &batch.values)
    };

    let mut out = PartialRows::empty(batch.str_cols.len(), batch.int_cols.len());
    for (gid, &row_idx) in reps.iter().enumerate() {
        out.append_from_batch(batch, row_idx, sums[gid]);
    }
    out
}

fn aggregate_partial_symbolized(rows: &PartialRows) -> PartialRows {
    let symbols = symbolize_cols(&rows.str_cols);
    let (reps, sums) = if rows.str_cols.len() == 2 && rows.int_cols.is_empty() {
        aggregate_packed_two_symbols(&symbols, &rows.sums)
    } else {
        aggregate_symbol_rows(&symbols, &rows.int_cols, &rows.sums)
    };

    let mut out = PartialRows::empty(rows.str_cols.len(), rows.int_cols.len());
    for (gid, &row_idx) in reps.iter().enumerate() {
        out.append_from_partial(rows, row_idx, sums[gid]);
    }
    out
}

fn run_daft_generic_staged(workload: &Workload) -> usize {
    let mut partials = PartialRows::empty(workload.num_str_cols, workload.num_int_cols);
    for batch in &workload.batches {
        partials.extend(aggregate_batch_generic(batch));
    }
    let final_rows = aggregate_partial_generic(&partials);
    black_box(final_rows.len())
}

fn run_daft_symbolized_staged(workload: &Workload) -> usize {
    let mut partials = PartialRows::empty(workload.num_str_cols, workload.num_int_cols);
    for batch in &workload.batches {
        partials.extend(aggregate_batch_symbolized(batch));
    }
    let final_rows = aggregate_partial_symbolized(&partials);
    black_box(final_rows.len())
}

fn run_taper_persistent(workload: &Workload) -> usize {
    let mut descs = Vec::new();
    for _ in 0..workload.num_str_cols { descs.push(ColumnDesc::Varchar); }
    for _ in 0..workload.num_int_cols { descs.push(ColumnDesc::Int64); }

    let initial_chunks = ((workload.expected_groups * 2).max(8) / 8 + 1).next_power_of_two();
    let mut table = TaperColumnSerializeHandler::new(&descs, 8, initial_chunks);

    for batch in &workload.batches {
        let hashes: Vec<u64> = (0..batch.len()).map(|i| hash_batch_row(batch, i)).collect();
        let str_slices: Vec<Vec<&[u8]>> = batch
            .str_cols.iter()
            .map(|col| col.iter().map(Vec::as_slice).collect())
            .collect();

        let mut columns = Vec::new();
        for col in &str_slices { columns.push(ColumnInput::Varchar(col)); }
        for col in &batch.int_cols { columns.push(ColumnInput::Int64(col)); }
        table.emplace_table_with_decode(&hashes, &columns, &batch.values);
    }

    black_box(table.num_groups())
}

fn bench_real_multibatch(c: &mut Criterion) {
    let mut group = c.benchmark_group("real_multibatch_varchar");
    group.sample_size(10);

    let scenarios = [
        ("2str_long_reuse", 2, 0, 32, 0.08),
        ("4str_long_reuse", 4, 0, 32, 0.08),
        ("2str_short_reuse", 2, 0, 8, 0.08),
        ("2str_2int_long_reuse", 2, 2, 32, 0.08),
        ("2str_long_mostly_new", 2, 0, 32, 0.75),
    ];

    for (name, num_str, num_int, string_len, new_key_rate) in scenarios {
        let mut rng = Mt19937GenRand64::new(42);
        let workload = generate_workload(
            16, 16_384, num_str, num_int, 131_072, new_key_rate, string_len, &mut rng,
        );
        let param = format!("{name}_batches=16_rows=16384_groups={}", workload.expected_groups);

        group.bench_with_input(BenchmarkId::new("omni_taper_persistent", &param), &workload, |b, w| {
            b.iter(|| run_taper_persistent(black_box(w)));
        });
        group.bench_with_input(BenchmarkId::new("daft_hashbrown_generic_staged", &param), &workload, |b, w| {
            b.iter(|| run_daft_generic_staged(black_box(w)));
        });
        group.bench_with_input(BenchmarkId::new("daft_hashbrown_symbolized_staged", &param), &workload, |b, w| {
            b.iter(|| run_daft_symbolized_staged(black_box(w)));
        });
    }

    group.finish();
}

criterion_group!(benches, bench_real_multibatch);
criterion_main!(benches);
