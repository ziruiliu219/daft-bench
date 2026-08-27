//! TaperColumnSerializeHandler: the full 5-step emplace pipeline.
//!
//! Mirrors C++ `TaperColumnSerializeHandler` in `column_marshaller.h`.
//! All varchar serialization/comparison methods are here (matching C++ class).

use crate::batch_compare::{
    batch_compare_decoded_i64, batch_compare_varchar_decoded,
    batch_compare_varchar_decoded_cached, build_merged_varchar_cache,
};
use crate::chunk::SlotValue;
use crate::row_container::{ColumnKind, RowContainer};
use crate::taper_hashmap::TaperHashMap;

// ═══════════════════════════════════════════════════════════════════════════════
// Column descriptors and input types
// ═══════════════════════════════════════════════════════════════════════════════

#[derive(Clone)]
pub enum ColumnDesc { Int64, Varchar }

pub enum ColumnInput<'a> {
    Int64(&'a [i64]),
    Varchar(&'a [&'a [u8]]),
}

// ═══════════════════════════════════════════════════════════════════════════════
// Static methods of TaperColumnSerializeHandler (varchar serialization/compare)
// ═══════════════════════════════════════════════════════════════════════════════

/// Compute `rowLenSize` for a string length.
/// Mirrors C++ logic in `SerializeVarcharToBuffer`.
#[inline(always)]
pub fn compute_row_len_size(string_len: usize) -> u8 {
    if string_len <= 0xFF { 1 }
    else if string_len <= 0xFFFF { 2 }
    else { 4 }
}

/// Serialize a non-null VARCHAR value into a buffer. Returns bytes written.
/// Mirrors C++ `TaperColumnSerializeHandler::SerializeVarcharToBuffer`.
#[inline]
pub fn serialize_varchar_to_buffer(write_pos: *mut u8, data: &[u8]) -> usize {
    let string_len = data.len();
    let row_len_size = compute_row_len_size(string_len);
    unsafe {
        *write_pos = row_len_size;
        let len_bytes = (string_len as u32).to_le_bytes();
        std::ptr::copy_nonoverlapping(len_bytes.as_ptr(), write_pos.add(1), row_len_size as usize);
        if string_len > 0 {
            std::ptr::copy_nonoverlapping(data.as_ptr(), write_pos.add(1 + row_len_size as usize), string_len);
        }
    }
    1 + row_len_size as usize + string_len
}

/// Serialize a null VARCHAR marker `[0]`. Returns bytes written (always 1).
/// Mirrors C++ `NullVariableTypeSerializer`.
#[inline]
pub fn null_variable_type_serializer(write_pos: *mut u8) -> usize {
    unsafe { *write_pos = 0; }
    1
}

/// Compute serialized size of a varchar entry from its arena pointer.
/// Mirrors C++ `TaperColumnSerializeHandler::ComputeVarCharSerializedSize`.
#[inline]
pub fn compute_varchar_serialized_size(data: *const u8) -> usize {
    unsafe {
        let row_len_size = *data;
        if row_len_size == 0 { return 1; }
        let string_len: usize = match row_len_size {
            1 => *data.add(1) as usize,
            2 => (data.add(1) as *const u16).read_unaligned() as usize,
            4 => (data.add(1) as *const u32).read_unaligned() as usize,
            _ => return 1,
        };
        1 + row_len_size as usize + string_len
    }
}

/// Compare varchar stored in arena format against input bytes. Returns true if equal.
/// Mirrors C++ `TaperColumnSerializeHandler::CompareVarcharFromRow`:
///   return memcmp(rowDataPtr, sv.data(), stringLen) == 0;
/// Uses slice equality which compiles to memcmp — identical to OmniOperator.
#[inline]
pub fn compare_varchar_from_row(arena_ptr: *const u8, input: &[u8]) -> bool {
    unsafe {
        let row_len_size = *arena_ptr;
        let string_len: usize = match row_len_size {
            1 => *arena_ptr.add(1) as usize,
            2 => (arena_ptr.add(1) as *const u16).read_unaligned() as usize,
            4 => (arena_ptr.add(1) as *const u32).read_unaligned() as usize,
            _ => return false,
        };
        if string_len != input.len() { return false; }
        if string_len == 0 { return true; }
        let data_ptr = arena_ptr.add(1 + row_len_size as usize);
        // mirrors: return memcmp(rowDataPtr, sv.data(), stringLen) == 0;
        std::slice::from_raw_parts(data_ptr, string_len) == input
    }
}

/// Read varchar pointer from a row. Mirrors C++ `*reinterpret_cast<char**>(row + offset)`.
#[inline(always)]
pub fn read_varchar_ptr(row: *const u8, col_offset: usize) -> *const u8 {
    unsafe { (row.add(col_offset) as *const *const u8).read_unaligned() }
}

/// Read varchar data from arena pointer. Parses `[rowLenSize][len][data]`.
#[inline]
pub fn read_varchar_from_arena(arena_ptr: *const u8) -> &'static [u8] {
    unsafe {
        let row_len_size = *arena_ptr;
        let string_len: usize = match row_len_size {
            1 => *arena_ptr.add(1) as usize,
            2 => (arena_ptr.add(1) as *const u16).read_unaligned() as usize,
            4 => (arena_ptr.add(1) as *const u32).read_unaligned() as usize,
            _ => 0,
        };
        std::slice::from_raw_parts(arena_ptr.add(1 + row_len_size as usize), string_len)
    }
}

/// Store single varchar into arena + write pointer to row.
/// Mirrors C++ `VariableTypeSerializer` + `*reinterpret_cast<char**>(row+offset) = ptr`.
#[inline]
pub fn variable_type_serializer(rc: &mut RowContainer, row: *mut u8, col_idx: usize, data: &[u8]) {
    let total_size = 1 + compute_row_len_size(data.len()) as usize + data.len();
    let arena_ptr = rc.arena_alloc(total_size);
    serialize_varchar_to_buffer(arena_ptr, data);
    let col_offset = rc.column_at(col_idx).offset();
    unsafe { (row.add(col_offset) as *mut *const u8).write_unaligned(arena_ptr as *const u8); }
}

/// Store null varchar: arena `[0]` + pointer + set null bit.
/// Mirrors C++ `NullVariableTypeSerializer` + SetNullAt.
#[inline]
pub fn null_variable_type_serializer_to_row(rc: &mut RowContainer, row: *mut u8, col_idx: usize) {
    let arena_ptr = rc.arena_alloc(1);
    null_variable_type_serializer(arena_ptr);
    let col_offset = rc.column_at(col_idx).offset();
    unsafe { (row.add(col_offset) as *mut *const u8).write_unaligned(arena_ptr as *const u8); }
    let col = rc.column_at(col_idx);
    RowContainer::set_null_at(row, col.null_byte(), col.null_mask());
}

/// Store merged varchar columns into one arena block.
/// Mirrors C++ `StoreKeyOneRowFromDecode` merged path.
pub fn store_merged_varchar_columns(
    rc: &mut RowContainer, row: *mut u8,
    varchar_col_indices: &[usize], varchar_slot_col_idx: usize,
    values: &[Option<&[u8]>],
) {
    assert_eq!(varchar_col_indices.len(), values.len());
    let mut total_size = 0usize;
    for val in values.iter() {
        match val {
            None => total_size += 1,
            Some(data) => { total_size += 1 + compute_row_len_size(data.len()) as usize + data.len(); }
        }
    }
    let block_start = rc.arena_alloc(total_size);
    let mut write_pos = block_start;
    for (i, val) in values.iter().enumerate() {
        let vc_idx = varchar_col_indices[i];
        let col = rc.column_at(vc_idx);
        match val {
            None => {
                RowContainer::set_null_at(row, col.null_byte(), col.null_mask());
                let written = null_variable_type_serializer(write_pos);
                write_pos = unsafe { write_pos.add(written) };
            }
            Some(data) => {
                RowContainer::clear_null_at(row, col.null_byte(), col.null_mask());
                let written = serialize_varchar_to_buffer(write_pos, data);
                write_pos = unsafe { write_pos.add(written) };
            }
        }
    }
    let slot_offset = rc.column_at(varchar_slot_col_idx).offset();
    unsafe { (row.add(slot_offset) as *mut *const u8).write_unaligned(block_start as *const u8); }
}

/// Get all merged varchar pointers from a row.
/// Mirrors C++ `TaperColumnSerializeHandler::GetAllMergedVarcharPtrs`.
pub fn get_all_merged_varchar_ptrs(
    row: *const u8, varchar_slot_col_offset: usize,
    varchar_col_descs: &[(usize, u8)], out_ptrs: &mut [*const u8],
) -> usize {
    let block_ptr = read_varchar_ptr(row, varchar_slot_col_offset);
    if block_ptr.is_null() {
        for p in out_ptrs.iter_mut() { *p = std::ptr::null(); }
        return varchar_col_descs.len();
    }
    let mut pos = block_ptr;
    for (i, &(null_byte, null_mask)) in varchar_col_descs.iter().enumerate() {
        if i >= out_ptrs.len() { break; }
        if RowContainer::is_null_at(row, null_byte, null_mask) {
            out_ptrs[i] = std::ptr::null();
            unsafe { pos = pos.add(1); }
        } else {
            out_ptrs[i] = pos;
            let entry_size = compute_varchar_serialized_size(pos);
            unsafe { pos = pos.add(entry_size); }
        }
    }
    varchar_col_descs.len()
}

// ═══════════════════════════════════════════════════════════════════════════════
// Standalone pipeline functions
// ═══════════════════════════════════════════════════════════════════════════════

/// Mirrors C++ `StoreKeyOneRowFromDecode`.
pub fn store_key_one_row_from_decode(
    rc: &mut RowContainer, row: *mut u8, row_idx: usize, columns: &[ColumnInput],
    col_descs: &[ColumnDesc], col_offsets: &[usize],
    varchar_col_indices: &[usize], varchar_slot_col_idx: usize, use_merged: bool,
) {
    // Store varchar columns — matches C++ StoreKeyOneRowFromDecode:
    // directly iterates varcharColIndices and serializes inline, no temporary array.
    if use_merged && !varchar_col_indices.is_empty() {
        // Compute total size
        let mut total_size = 0usize;
        for &vc_idx in varchar_col_indices.iter() {
            let data = match &columns[vc_idx] { ColumnInput::Varchar(v) => v[row_idx], _ => panic!("") };
            total_size += 1 + compute_row_len_size(data.len()) as usize + data.len();
        }
        // Allocate one block
        let block_start = rc.arena_alloc(total_size);
        let mut write_pos = block_start;
        // Serialize each column directly (no Vec allocation)
        for &vc_idx in varchar_col_indices.iter() {
            let col = rc.column_at(vc_idx);
            let data = match &columns[vc_idx] { ColumnInput::Varchar(v) => v[row_idx], _ => panic!("") };
            RowContainer::clear_null_at(row, col.null_byte(), col.null_mask());
            let written = serialize_varchar_to_buffer(write_pos, data);
            write_pos = unsafe { write_pos.add(written) };
        }
        // Store pointer in slot column
        let slot_offset = rc.column_at(varchar_slot_col_idx).offset();
        unsafe { (row.add(slot_offset) as *mut *const u8).write_unaligned(block_start as *const u8); }
    } else if varchar_col_indices.len() == 1 {
        let vc_idx = varchar_col_indices[0];
        match &columns[vc_idx] {
            ColumnInput::Varchar(v) => variable_type_serializer(rc, row, vc_idx, v[row_idx]),
            _ => panic!(""),
        }
    }
    // Store int columns
    for (col_idx, desc) in col_descs.iter().enumerate() {
        if let ColumnDesc::Int64 = desc {
            match &columns[col_idx] {
                ColumnInput::Int64(v) => RowContainer::store_value::<i64>(row, col_offsets[col_idx], v[row_idx]),
                _ => {}
            }
        }
    }
}

/// Mirrors C++ `GetUnequalsNumWithDecode`.
pub fn get_unequals_num_with_decode(
    working_indices: &mut [u32], count: usize, columns: &[ColumnInput],
    groups: &[*const u8], col_descs: &[ColumnDesc], col_offsets: &[usize],
    varchar_col_indices: &[usize], use_merged: bool,
    varchar_slot_col_offset: usize, varchar_col_descs: &[(usize, u8)],
) -> usize {
    let num_varchar = varchar_col_indices.len();
    let merged_cache = if use_merged && num_varchar > 0 {
        let max_idx = *working_indices[..count].iter().max().unwrap_or(&0) as usize;
        Some(build_merged_varchar_cache(groups, &working_indices[..count], count, varchar_slot_col_offset, varchar_col_descs, max_idx))
    } else { None };

    let mut idx_from = 0usize;
    for (col_idx, desc) in col_descs.iter().enumerate() {
        if idx_from >= count { break; }
        let remaining = count - idx_from;
        match desc {
            ColumnDesc::Int64 => {
                let input = match &columns[col_idx] { ColumnInput::Int64(v) => *v, _ => panic!("") };
                idx_from += batch_compare_decoded_i64(&mut working_indices[idx_from..], remaining, input, groups, col_offsets[col_idx]);
            }
            ColumnDesc::Varchar => {
                let input = match &columns[col_idx] { ColumnInput::Varchar(v) => *v, _ => panic!("") };
                if use_merged && num_varchar > 1 {
                    let vc_pos = varchar_col_indices.iter().position(|&c| c == col_idx).unwrap();
                    idx_from += batch_compare_varchar_decoded_cached(&mut working_indices[idx_from..], remaining, input, merged_cache.as_ref().unwrap(), num_varchar, vc_pos);
                } else {
                    idx_from += batch_compare_varchar_decoded(&mut working_indices[idx_from..], remaining, input, groups, col_offsets[col_idx]);
                }
            }
        }
    }
    idx_from
}

/// Mirrors C++ `CompareKeysWithDecode`.
pub fn compare_keys_with_decode(
    row_ptr: *const u8, row_idx: usize, columns: &[ColumnInput],
    col_descs: &[ColumnDesc], col_offsets: &[usize],
) -> bool {
    for (col_idx, desc) in col_descs.iter().enumerate() {
        match desc {
            ColumnDesc::Int64 => {
                let stored: i64 = RowContainer::read_value::<i64>(row_ptr, col_offsets[col_idx]);
                let input = match &columns[col_idx] { ColumnInput::Int64(v) => v[row_idx], _ => panic!("") };
                if stored != input { return false; }
            }
            ColumnDesc::Varchar => {
                let input = match &columns[col_idx] { ColumnInput::Varchar(v) => v[row_idx], _ => panic!("") };
                let arena_ptr = read_varchar_ptr(row_ptr, col_offsets[col_idx]);
                if arena_ptr.is_null() || unsafe { *arena_ptr == 0 } { return false; }
                if !compare_varchar_from_row(arena_ptr, input) { return false; }
            }
        }
    }
    true
}

// ═══════════════════════════════════════════════════════════════════════════════
// TaperColumnSerializeHandler struct
// ═══════════════════════════════════════════════════════════════════════════════

pub struct TaperColumnSerializeHandler {
    map: TaperHashMap,
    rc: RowContainer,
    col_descs: Vec<ColumnDesc>,
    col_offsets: Vec<usize>,
    agg_offset: usize,
    varchar_col_indices: Vec<usize>,
    varchar_slot_col_idx: usize,
    use_merged: bool,
    varchar_col_descs: Vec<(usize, u8)>,
    varchar_slot_col_offset: usize,
    // Reusable buffers (mirrors C++ class members)
    groups: Vec<*const u8>,
    update_indices: Vec<u32>,
    merged_cache: Vec<*const u8>,
}

impl TaperColumnSerializeHandler {
    pub fn new(columns: &[ColumnDesc], agg_state_size: usize, initial_capacity: usize) -> Self {
        let num_cols = columns.len();
        let mut key_sizes = Vec::with_capacity(num_cols);
        let mut kinds = Vec::with_capacity(num_cols);
        let mut varchar_col_indices = Vec::new();
        for (i, col) in columns.iter().enumerate() {
            match col {
                ColumnDesc::Int64 => { key_sizes.push(8); kinds.push(ColumnKind::Fixed); }
                ColumnDesc::Varchar => { key_sizes.push(0); kinds.push(ColumnKind::Varchar); varchar_col_indices.push(i); }
            }
        }
        let rc = RowContainer::with_kinds(&key_sizes, &kinds, agg_state_size);
        let col_offsets: Vec<usize> = (0..num_cols).map(|c| rc.column_at(c).offset()).collect();
        let agg_offset = rc.agg_state_offset();
        let use_merged = varchar_col_indices.len() > 1;
        let varchar_slot_col_idx = varchar_col_indices.first().copied().unwrap_or(0);
        let varchar_slot_col_offset = if !varchar_col_indices.is_empty() { col_offsets[varchar_slot_col_idx] } else { 0 };
        let varchar_col_descs: Vec<(usize, u8)> = varchar_col_indices.iter()
            .map(|&c| { let col = rc.column_at(c); (col.null_byte(), col.null_mask()) }).collect();

        TaperColumnSerializeHandler {
            map: TaperHashMap::with_capacity(initial_capacity), rc,
            col_descs: columns.to_vec(), col_offsets, agg_offset,
            varchar_col_indices, varchar_slot_col_idx, use_merged, varchar_col_descs, varchar_slot_col_offset,
            groups: Vec::new(), update_indices: Vec::new(), merged_cache: Vec::new(),
        }
    }

    pub fn num_groups(&self) -> usize { self.rc.num_rows() }

    /// Number of chunks in the underlying hash table (grows on rehash).
    pub fn map_num_chunks(&self) -> usize { self.map.num_chunks() }

    /// Output all group results: reads agg values from all rows in RowContainer.
    /// Mirrors the real engine's "output result to downstream operator" step.
    /// Returns the sum of all agg values (as a checksum to prevent optimization).
    pub fn output_agg_checksum(&self) -> i64 {
        self.rc.agg_i64_checksum(self.agg_offset)
    }

    /// Mirrors C++ `EmplaceTableWithDecode`.
    pub fn emplace_table_with_decode(&mut self, hashes: &[u64], columns: &[ColumnInput], agg_values: &[i64]) {
        assert_eq!(columns.len(), self.col_descs.len());
        let n = hashes.len();
        if n == 0 { return; }

        // No per-batch reserve: emplace_batch_impl rehashes internally when the
        // load factor is exceeded (matches OmniOperator ExpandCapacityIteratively).

        // Reuse buffers (mirrors C++ class member resize pattern)
        self.groups.resize(n, std::ptr::null());
        unsafe { std::ptr::write_bytes(self.groups.as_mut_ptr(), 0, n); }
        self.update_indices.clear();

        // Step 2+3
        let rc_ptr = &mut self.rc as *mut RowContainer;
        let col_offsets = &self.col_offsets as *const Vec<usize>;
        let col_descs = &self.col_descs as *const Vec<ColumnDesc>;
        let varchar_col_indices = &self.varchar_col_indices as *const Vec<usize>;
        let varchar_slot_col_idx = self.varchar_slot_col_idx;
        let use_merged = self.use_merged;
        let agg_offset = self.agg_offset;
        let groups_ptr = self.groups.as_mut_ptr();
        let update_indices_ptr = &mut self.update_indices as *mut Vec<u32>;

        self.map.emplace_batch_impl(
            hashes,
            &|_: usize, _: &SlotValue| -> bool { true },
            &mut |i: usize, sv: &mut SlotValue| {
                let rc = unsafe { &mut *rc_ptr };
                let row = rc.new_row();
                unsafe {
                    store_key_one_row_from_decode(rc, row, i, columns, &*col_descs, &*col_offsets, &*varchar_col_indices, varchar_slot_col_idx, use_merged);
                    RowContainer::store_value::<i64>(row, agg_offset, agg_values[i]);
                    *groups_ptr.add(i) = row as *const u8;
                }
                sv.set_ptr(row as *const u8);
            },
            &mut |i: usize, sv: &SlotValue, is_new: bool| {
                if !is_new { unsafe { *groups_ptr.add(i) = sv.get_ptr(); (*update_indices_ptr).push(i as u32); } }
            },
        );

        // Step 4: GetUnequalsNumWithDecode — mirrors C++ method of same name
        let count = self.update_indices.len();
        if count == 0 { return; }
        let mut working_indices = std::mem::take(&mut self.update_indices);
        let idx_from = self.get_unequals_num_with_decode(&mut working_indices, count, columns);

        // Step 5
        for ui in 0..idx_from {
            let row_idx = working_indices[ui] as usize;
            let hash = hashes[row_idx];
            let col_descs_ref = &self.col_descs;
            let col_offsets_ref = &self.col_offsets;
            let varchar_col_indices_ref = &self.varchar_col_indices;
            let vc_slot_idx = self.varchar_slot_col_idx;
            let merged = self.use_merged;
            let agg_off = self.agg_offset;
            let rc_ptr2 = &mut self.rc as *mut RowContainer;
            let key_cmp = |sv: &SlotValue| -> bool { compare_keys_with_decode(sv.get_ptr(), row_idx, columns, col_descs_ref, col_offsets_ref) };
            let mut on_init = |sv: &mut SlotValue| {
                let rc = unsafe { &mut *rc_ptr2 };
                let row = rc.new_row();
                store_key_one_row_from_decode(rc, row, row_idx, columns, col_descs_ref, col_offsets_ref, varchar_col_indices_ref, vc_slot_idx, merged);
                RowContainer::store_value::<i64>(row, agg_off, agg_values[row_idx]);
                sv.set_ptr(row as *const u8);
            };
            let mut on_update = |sv: &SlotValue, is_new: bool| {
                if !is_new { let rp = sv.get_ptr() as *mut u8; unsafe { *(rp.add(agg_off) as *mut i64) += agg_values[row_idx]; } }
            };
            self.map.emplace(hash, &key_cmp, &mut on_init, &mut on_update);
        }

        // Accumulate agg for equal rows
        for ui in idx_from..count {
            let row_idx = working_indices[ui] as usize;
            let rp = self.groups[row_idx] as *mut u8;
            unsafe { *(rp.add(self.agg_offset) as *mut i64) += agg_values[row_idx]; }
        }
        self.update_indices = working_indices;
    }

    /// Mirrors C++ `TaperColumnSerializeHandler::GetUnequalsNumWithDecode`.
    #[inline(never)]
    fn get_unequals_num_with_decode(
        &mut self, working_indices: &mut [u32], count: usize, columns: &[ColumnInput],
    ) -> usize {
        let num_varchar = self.varchar_col_indices.len();

        // Build merged varchar cache
        if self.use_merged && num_varchar > 0 {
            let max_idx = *working_indices[..count].iter().max().unwrap_or(&0) as usize;
            let cache_size = (max_idx + 1) * num_varchar;
            self.merged_cache.resize(cache_size, std::ptr::null());
            unsafe { std::ptr::write_bytes(self.merged_cache.as_mut_ptr(), 0, cache_size); }
            let mut out_ptrs = vec![std::ptr::null::<u8>(); num_varchar];
            for wi in 0..count {
                let idx = working_indices[wi] as usize;
                get_all_merged_varchar_ptrs(
                    self.groups[idx], self.varchar_slot_col_offset,
                    &self.varchar_col_descs, &mut out_ptrs,
                );
                for vc in 0..num_varchar {
                    self.merged_cache[idx * num_varchar + vc] = out_ptrs[vc];
                }
            }
        }

        let mut idx_from = 0usize;
        for (col_idx, desc) in self.col_descs.iter().enumerate() {
            if idx_from >= count { break; }
            let remaining = count - idx_from;
            match desc {
                ColumnDesc::Int64 => {
                    let input = match &columns[col_idx] {
                        ColumnInput::Int64(v) => *v, _ => panic!("")
                    };
                    idx_from += batch_compare_decoded_i64(
                        &mut working_indices[idx_from..], remaining,
                        input, &self.groups, self.col_offsets[col_idx],
                    );
                }
                ColumnDesc::Varchar => {
                    let input = match &columns[col_idx] {
                        ColumnInput::Varchar(v) => *v, _ => panic!("")
                    };
                    if self.use_merged && num_varchar > 1 {
                        let vc_pos = self.varchar_col_indices.iter()
                            .position(|&c| c == col_idx).unwrap();
                        idx_from += batch_compare_varchar_decoded_cached(
                            &mut working_indices[idx_from..], remaining,
                            input, &self.merged_cache, num_varchar, vc_pos,
                        );
                    } else {
                        idx_from += batch_compare_varchar_decoded(
                            &mut working_indices[idx_from..], remaining,
                            input, &self.groups, self.col_offsets[col_idx],
                        );
                    }
                }
            }
        }
        idx_from
    }
}
