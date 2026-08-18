/// Batch key comparison (Stage 2): compare input values vs RowContainer stored values.
/// Returns number of unequal rows. Unequal row indices are moved to the front of `indices`.

use crate::column_marshaller::{read_varchar_ptr, compare_varchar_from_row, get_all_merged_varchar_ptrs};
use crate::row_container::RowContainer;

/// Scalar implementation: works on all platforms.
pub fn batch_compare_decoded_i64_scalar(
    indices: &mut [u32],
    count: usize,
    input_values: &[i64],
    groups: &[*const u8],
    offset: usize,
) -> usize {
    let mut idx_from = 0;

    for i in 0..count {
        let idx = indices[i] as usize;
        let row = groups[idx];
        let stored: i64 = RowContainer::read_value::<i64>(row, offset);
        let input = input_values[idx];

        if stored != input {
            indices.swap(i, idx_from);
            idx_from += 1;
        }
    }

    idx_from
}

/// NEON SIMD implementation for aarch64 (compare multiple rows in parallel).
#[cfg(target_arch = "aarch64")]
pub fn batch_compare_decoded_i64_neon(
    indices: &mut [u32],
    count: usize,
    input_values: &[i64],
    groups: &[*const u8],
    offset: usize,
) -> usize {
    use std::arch::aarch64::*;

    let mut idx_from = 0;

    // Process 2 rows at a time using NEON 128-bit (2 × i64)
    let mut i = 0;
    while i + 2 <= count {
        let idx0 = indices[i] as usize;
        let idx1 = indices[i + 1] as usize;

        let row0 = groups[idx0];
        let row1 = groups[idx1];

        unsafe {
            let stored0: i64 = RowContainer::read_value::<i64>(row0, offset);
            let stored1: i64 = RowContainer::read_value::<i64>(row1, offset);
            let input0 = input_values[idx0];
            let input1 = input_values[idx1];

            let v_stored = vcombine_s64(vcreate_s64(stored0 as u64), vcreate_s64(stored1 as u64));
            let v_input = vcombine_s64(vcreate_s64(input0 as u64), vcreate_s64(input1 as u64));

            // Compare: element-wise equal
            let cmp = vceqq_s64(v_stored, v_input);
            let mask0 = vgetq_lane_u64::<0>(cmp);
            let mask1 = vgetq_lane_u64::<1>(cmp);

            if mask0 == 0 {  // not equal
                indices.swap(i, idx_from);
                idx_from += 1;
            }
            if mask1 == 0 {  // not equal
                indices.swap(i + 1, idx_from);
                idx_from += 1;
            }
        }
        i += 2;
    }

    // Handle remaining row
    while i < count {
        let idx = indices[i] as usize;
        let row = groups[idx];
        let stored: i64 = RowContainer::read_value::<i64>(row, offset);
        let input = input_values[idx];
        if stored != input {
            indices.swap(i, idx_from);
            idx_from += 1;
        }
        i += 1;
    }

    idx_from
}

/// NEON SIMD implementation for i32 on aarch64 (compare 4 rows in parallel).
#[cfg(target_arch = "aarch64")]
pub fn batch_compare_decoded_i32_neon(
    indices: &mut [u32],
    count: usize,
    input_values: &[i32],
    groups: &[*const u8],
    offset: usize,
) -> usize {
    use std::arch::aarch64::*;

    let mut idx_from = 0;

    // Process 4 rows at a time using NEON 128-bit (4 × i32)
    let mut i = 0;
    while i + 4 <= count {
        let idx0 = indices[i] as usize;
        let idx1 = indices[i + 1] as usize;
        let idx2 = indices[i + 2] as usize;
        let idx3 = indices[i + 3] as usize;

        unsafe {
            let stored0: i32 = RowContainer::read_value::<i32>(groups[idx0], offset);
            let stored1: i32 = RowContainer::read_value::<i32>(groups[idx1], offset);
            let stored2: i32 = RowContainer::read_value::<i32>(groups[idx2], offset);
            let stored3: i32 = RowContainer::read_value::<i32>(groups[idx3], offset);

            let v_stored = vcombine_s32(
                vcreate_s32(((stored1 as u32 as u64) << 32) | (stored0 as u32 as u64)),
                vcreate_s32(((stored3 as u32 as u64) << 32) | (stored2 as u32 as u64)),
            );
            let v_input = vcombine_s32(
                vcreate_s32(((input_values[idx1] as u32 as u64) << 32) | (input_values[idx0] as u32 as u64)),
                vcreate_s32(((input_values[idx3] as u32 as u64) << 32) | (input_values[idx2] as u32 as u64)),
            );

            let cmp = vceqq_s32(v_stored, v_input);
            let mask0 = vgetq_lane_u32::<0>(cmp);
            let mask1 = vgetq_lane_u32::<1>(cmp);
            let mask2 = vgetq_lane_u32::<2>(cmp);
            let mask3 = vgetq_lane_u32::<3>(cmp);

            if mask0 == 0 { indices.swap(i, idx_from); idx_from += 1; }
            if mask1 == 0 { indices.swap(i + 1, idx_from); idx_from += 1; }
            if mask2 == 0 { indices.swap(i + 2, idx_from); idx_from += 1; }
            if mask3 == 0 { indices.swap(i + 3, idx_from); idx_from += 1; }
        }
        i += 4;
    }

    // Handle remaining rows
    while i < count {
        let idx = indices[i] as usize;
        let row = groups[idx];
        let stored: i32 = RowContainer::read_value::<i32>(row, offset);
        let input = input_values[idx];
        if stored != input {
            indices.swap(i, idx_from);
            idx_from += 1;
        }
        i += 1;
    }

    idx_from
}

/// Scalar implementation for i32.
pub fn batch_compare_decoded_i32_scalar(
    indices: &mut [u32],
    count: usize,
    input_values: &[i32],
    groups: &[*const u8],
    offset: usize,
) -> usize {
    let mut idx_from = 0;
    for i in 0..count {
        let idx = indices[i] as usize;
        let row = groups[idx];
        let stored: i32 = RowContainer::read_value::<i32>(row, offset);
        if stored != input_values[idx] {
            indices.swap(i, idx_from);
            idx_from += 1;
        }
    }
    idx_from
}

/// Dispatch i32 comparison to best available implementation.
pub fn batch_compare_decoded_i32(
    indices: &mut [u32],
    count: usize,
    input_values: &[i32],
    groups: &[*const u8],
    offset: usize,
) -> usize {
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(debug_assertions)]
        eprintln!("[SIMD] batch_compare_decoded_i32: using NEON (vceqq_s32, 4×i32/iter)");
        batch_compare_decoded_i32_neon(indices, count, input_values, groups, offset)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        #[cfg(debug_assertions)]
        eprintln!("[SCALAR] batch_compare_decoded_i32: NEON not available, using scalar");
        batch_compare_decoded_i32_scalar(indices, count, input_values, groups, offset)
    }
}

/// Dispatch to best available implementation.
pub fn batch_compare_decoded_i64(
    indices: &mut [u32],
    count: usize,
    input_values: &[i64],
    groups: &[*const u8],
    offset: usize,
) -> usize {
    #[cfg(target_arch = "aarch64")]
    {
        #[cfg(debug_assertions)]
        eprintln!("[SIMD] batch_compare_decoded_i64: using NEON (vceqq_s64, 2×i64/iter)");
        batch_compare_decoded_i64_neon(indices, count, input_values, groups, offset)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        #[cfg(debug_assertions)]
        eprintln!("[SCALAR] batch_compare_decoded_i64: NEON not available, using scalar");
        batch_compare_decoded_i64_scalar(indices, count, input_values, groups, offset)
    }
}

// ─── Varchar (variable-length byte slice) comparison ────────────────────────
// Mirrors C++ `BatchCompareVarcharDecoded` + `CompareVarcharFromRow`.
//
// Row stores a `*const u8` pointer at `col_offset` pointing to arena data.
// Arena format: [rowLenSize:1B][length:rowLenSize B][data:length B]
//
// Comparison reads the pointer from the row, then calls
// `RowContainer::compare_varchar_from_row` which parses the arena format
// and does byte comparison — identical to C++ `CompareVarcharFromRow`.

/// Batch compare varchar keys: compare arena-stored varchar values against input byte slices.
///
/// Each row in `groups` has a pointer at `col_offset` → arena data in C++ format.
/// `input_values`: input byte slices indexed by row index.
///
/// Returns number of unequal rows. Unequal indices moved to front of `indices`.
///
/// Mirrors C++ `BatchCompareVarcharDecoded` (no-null, flat vector path).
pub fn batch_compare_varchar_decoded(
    indices: &mut [u32],
    count: usize,
    input_values: &[&[u8]],
    groups: &[*const u8],
    col_offset: usize,
) -> usize {
    let mut idx_from = 0;

    for i in 0..count {
        let idx = indices[i] as usize;
        let row = groups[idx];

        // Read pointer from row — mirrors C++: `*reinterpret_cast<char**>(row + offset)`
        let arena_ptr = read_varchar_ptr(row, col_offset);

        // Null pointer or rowLenSize == 0 means null/invalid → unequal
        if arena_ptr.is_null() {
            indices.swap(i, idx_from);
            idx_from += 1;
            continue;
        }

        unsafe {
            if *arena_ptr == 0 {
                // rowLenSize == 0 → null marker
                indices.swap(i, idx_from);
                idx_from += 1;
                continue;
            }
        }

        let input = input_values[idx];
        if !compare_varchar_from_row(arena_ptr, input) {
            indices.swap(i, idx_from);
            idx_from += 1;
        }
    }

    idx_from
}

/// Batch compare varchar using pre-computed merged varchar cache pointers.
///
/// Instead of reading the pointer from `row + col_offset`, uses a pre-populated
/// cache of per-column arena pointers (avoids repeated block walking for multiple
/// varchar columns).
///
/// Mirrors C++ `mergedVarcharCache_` usage in `BatchCompareVarcharDecoded`:
/// ```cpp
/// auto getVarcharData = [&](uint8_t* row, int32_t groupIdx) -> const uint8_t* {
///     if (!mergedVarcharCache_.empty()) {
///         int32_t vcPos = colToVarcharPos_[colIdx];
///         return mergedVarcharCache_[groupIdx * mergedVarcharCacheCount_ + vcPos];
///     }
///     ...
/// };
/// ```
///
/// `cache`: flat array indexed by `[row_idx * num_varchar_cols + varchar_pos]`
/// `varchar_pos`: position of this column in the merged varchar column list
/// `num_varchar_cols`: total number of merged varchar columns
pub fn batch_compare_varchar_decoded_cached(
    indices: &mut [u32],
    count: usize,
    input_values: &[&[u8]],
    cache: &[*const u8],
    num_varchar_cols: usize,
    varchar_pos: usize,
) -> usize {
    let mut idx_from = 0;

    for i in 0..count {
        let idx = indices[i] as usize;

        // Read from cache instead of row
        let arena_ptr = cache[idx * num_varchar_cols + varchar_pos];

        if arena_ptr.is_null() {
            // null → unequal (row was null for this column)
            indices.swap(i, idx_from);
            idx_from += 1;
            continue;
        }

        unsafe {
            if *arena_ptr == 0 {
                indices.swap(i, idx_from);
                idx_from += 1;
                continue;
            }
        }

        let input = input_values[idx];
        if !compare_varchar_from_row(arena_ptr, input) {
            indices.swap(i, idx_from);
            idx_from += 1;
        }
    }

    idx_from
}

/// Pre-populate merged varchar pointer cache for all rows in the update list.
/// This avoids re-walking the merged block for each varchar column during comparison.
///
/// Mirrors C++ `GetUnequalsNumWithDecode` preamble:
/// ```cpp
/// mergedVarcharCache_.resize((maxIdx + 1) * mergedVarcharCacheCount_);
/// for (int32_t wi = 0; wi < count; ++wi) {
///     int32_t idx = workingUpdateIndices[wi];
///     GetAllMergedVarcharPtrs(groups[idx], &cache[idx * count], count);
/// }
/// ```
///
/// `groups`: row pointers
/// `indices`: active row indices to cache
/// `count`: number of active indices
/// `varchar_slot_col_offset`: offset of the merged pointer slot in row
/// `varchar_col_descs`: (null_byte, null_mask) for each varchar column
/// `max_row_idx`: maximum row index (determines cache array size)
///
/// Returns a flat Vec where `cache[row_idx * num_vc_cols + vc_pos]` = arena pointer.
pub fn build_merged_varchar_cache(
    groups: &[*const u8],
    indices: &[u32],
    count: usize,
    varchar_slot_col_offset: usize,
    varchar_col_descs: &[(usize, u8)],
    max_row_idx: usize,
) -> Vec<*const u8> {
    let num_vc_cols = varchar_col_descs.len();
    let mut cache = vec![std::ptr::null::<u8>(); (max_row_idx + 1) * num_vc_cols];
    let mut out_ptrs = vec![std::ptr::null::<u8>(); num_vc_cols];

    for i in 0..count {
        let idx = indices[i] as usize;
        let row = groups[idx];
        get_all_merged_varchar_ptrs(
            row,
            varchar_slot_col_offset,
            varchar_col_descs,
            &mut out_ptrs,
        );
        for vc in 0..num_vc_cols {
            cache[idx * num_vc_cols + vc] = out_ptrs[vc];
        }
    }

    cache
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compare_scalar_basic() {
        // Simulate: 3 rows in updateList, one has a mismatch
        let mut pool = vec![0u8; 100];
        let offset = 0;

        // Row 0: stored value = 42
        unsafe { *(pool.as_mut_ptr().add(0) as *mut i64) = 42; }
        // Row 1: stored value = 77
        unsafe { *(pool.as_mut_ptr().add(16) as *mut i64) = 77; }
        // Row 2: stored value = 42 (but input will be different)
        unsafe { *(pool.as_mut_ptr().add(32) as *mut i64) = 42; }

        let groups: Vec<*const u8> = vec![
            pool.as_ptr(),
            unsafe { pool.as_ptr().add(16) },
            unsafe { pool.as_ptr().add(32) },
        ];

        let input_values: Vec<i64> = vec![42, 77, 99];  // row2 mismatch!
        let mut indices: Vec<u32> = vec![0, 1, 2];

        let unequals = batch_compare_decoded_i64_scalar(&mut indices, 3, &input_values, &groups, offset);

        assert_eq!(unequals, 1);
        assert_eq!(indices[0], 2);  // row 2 is the unequal one
    }

    #[test]
    fn test_simd_dispatch_i64() {
        // This test goes through the dispatch function to verify SIMD path is used on ARM
        let mut pool = vec![0u8; 64];
        unsafe {
            *(pool.as_mut_ptr().add(0) as *mut i64) = 100;
            *(pool.as_mut_ptr().add(16) as *mut i64) = 200;
            *(pool.as_mut_ptr().add(32) as *mut i64) = 300;
        }
        let groups: Vec<*const u8> = vec![
            pool.as_ptr(),
            unsafe { pool.as_ptr().add(16) },
            unsafe { pool.as_ptr().add(32) },
        ];
        let input_values: Vec<i64> = vec![100, 999, 300]; // row1 mismatch
        let mut indices: Vec<u32> = vec![0, 1, 2];

        // This calls the dispatch fn which prints SIMD/SCALAR flag in debug mode
        let unequals = batch_compare_decoded_i64(&mut indices, 3, &input_values, &groups, 0);
        assert_eq!(unequals, 1);
        assert_eq!(indices[0], 1);
    }
}

