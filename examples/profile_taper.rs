//! Quick profiling binary: runs taper on 4str_0int_ht=65536 multiple times for sampling.
use taper_hashmap::column_marshaller::{TaperColumnSerializeHandler, ColumnDesc, ColumnInput};
use xxhash_rust::xxh3::xxh3_64_with_seed;
use rand_mt::Mt19937GenRand64;

fn hash_bytes(data: &[u8], seed: u64) -> u64 { xxh3_64_with_seed(data, seed) }
fn hash_combine(seed: u64, val: i64) -> u64 { xxh3_64_with_seed(&val.to_le_bytes(), seed) }

fn main() {
    let num_str = 4usize;
    let num_int = 0usize;
    let ht_size = 65536usize;
    let num_keys = (ht_size as f64 * 0.5) as usize;
    let num_probe = 1_000_000usize;
    let selectivity = 0.5f64;
    let mut rng = Mt19937GenRand64::new(42);

    // Generate data (same logic as benchmark)
    let build_str: Vec<Vec<Vec<u8>>> = (0..num_str)
        .map(|c| (0..num_keys).map(|i| format!("key_{}_c{}", i, c).into_bytes()).collect())
        .collect();
    let build_hashes: Vec<u64> = (0..num_keys).map(|i| {
        let mut h = 0u64;
        for c in 0..num_str { h = hash_bytes(&build_str[c][i], h); }
        h
    }).collect();
    let build_values: Vec<i64> = (0..num_keys).map(|i| (i % 1000) as i64).collect();

    let num_hits = (num_probe as f64 * selectivity) as usize;
    let num_misses = num_probe - num_hits;
    let mut probe_str: Vec<Vec<Vec<u8>>> = vec![Vec::with_capacity(num_probe); num_str];
    let mut probe_hashes: Vec<u64> = Vec::with_capacity(num_probe);

    for _ in 0..num_hits {
        let idx = (rng.next_u64() as usize) % num_keys;
        for c in 0..num_str { probe_str[c].push(build_str[c][idx].clone()); }
        probe_hashes.push(build_hashes[idx]);
    }
    for i in 0..num_misses {
        let mut h = 0u64;
        for c in 0..num_str {
            let s = format!("miss_{}_{}", i, c).into_bytes();
            h = hash_bytes(&s, h);
            probe_str[c].push(s);
        }
        probe_hashes.push(h);
    }

    // Shuffle
    let mut order: Vec<usize> = (0..num_probe).collect();
    for i in (1..num_probe).rev() { order.swap(i, (rng.next_u64() as usize) % (i + 1)); }
    let probe_str: Vec<Vec<Vec<u8>>> = (0..num_str).map(|c| order.iter().map(|&i| probe_str[c][i].clone()).collect()).collect();
    let probe_hashes: Vec<u64> = order.iter().map(|&i| probe_hashes[i]).collect();
    let probe_values: Vec<i64> = (0..num_probe).map(|i| (i % 1000) as i64).collect();

    // Combine
    let mut all_str = build_str;
    for c in 0..num_str { all_str[c].extend(probe_str[c].clone()); }
    let mut all_hashes = build_hashes; all_hashes.extend_from_slice(&probe_hashes);
    let mut all_values = build_values; all_values.extend_from_slice(&probe_values);

    let str_slices: Vec<Vec<&[u8]>> = (0..num_str).map(|c| all_str[c].iter().map(|s| s.as_slice()).collect()).collect();

    // Run 20 iterations for profiling
    for _ in 0..20 {
        let mut col_descs: Vec<ColumnDesc> = Vec::new();
        for _ in 0..num_str { col_descs.push(ColumnDesc::Varchar); }
        for _ in 0..num_int { col_descs.push(ColumnDesc::Int64); }
        let mut table = TaperColumnSerializeHandler::new(&col_descs, 8, ht_size);

        let mut columns: Vec<ColumnInput> = Vec::new();
        for c in 0..num_str { columns.push(ColumnInput::Varchar(&str_slices[c])); }

        table.emplace_table_with_decode(&all_hashes, &columns, &all_values);
        std::hint::black_box(table.num_groups());
    }

    println!("Done. {} groups created per iteration.", num_keys + num_misses);
}
