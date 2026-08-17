//! RowContainer: row-oriented storage for group keys + aggregation state.
//!
//! Mirrors C++ `RowContainer` in `row_container.h`.
//! Only contains methods that exist in the C++ RowContainer class:
//!   - NewRow, IsNullAt, SetNullAt, ClearNullAt, ReadValue, StoreValue
//!   - ColumnAt, AggStateOffset, FixedRowSize, NumKeys, NumRows
//!
//! VARCHAR serialization/comparison methods belong to TaperColumnSerializeHandler
//! (in column_marshaller.rs), matching the C++ class hierarchy.

/// Packed representation of offset, null byte offset and null mask for a column.
/// Mirrors C++ `RowColumn`.
#[derive(Clone, Copy)]
pub struct RowColumn {
    packed: u64,
}

impl RowColumn {
    pub(crate) fn pack(offset: usize, null_bit_offset: usize, null_block_start: usize) -> Self {
        let null_byte = null_block_start + null_bit_offset / 8;
        let null_mask: u8 = 1 << (null_bit_offset & 7);
        let packed = ((offset as u64) << 32) | ((null_byte as u64) << 8) | (null_mask as u64);
        RowColumn { packed }
    }

    #[inline(always)]
    pub fn offset(&self) -> usize { (self.packed >> 32) as usize }

    #[inline(always)]
    pub fn null_byte(&self) -> usize { ((self.packed >> 8) & 0x00FF_FFFF) as usize }

    #[inline(always)]
    pub fn null_mask(&self) -> u8 { (self.packed & 0xFF) as u8 }
}

const BLOCK_ROWS: usize = 1024;

/// Column kind (used by TaperColumnSerializeHandler to dispatch logic).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColumnKind {
    Fixed,
    Varchar,
}

/// Size of pointer slot for varchar columns in row.
const VARCHAR_SLOT_SIZE: usize = std::mem::size_of::<*const u8>();

/// Block size for varchar arena.
const VARCHAR_ARENA_BLOCK_SIZE: usize = 64 * 1024;

/// RowContainer: stores group rows in fixed-size blocks.
/// Mirrors C++ `RowContainer`.
pub struct RowContainer {
    blocks: Vec<Vec<u8>>,
    row_size: usize,
    columns: Vec<RowColumn>,
    column_kinds: Vec<ColumnKind>,
    #[allow(dead_code)]
    null_block_start: usize,
    #[allow(dead_code)]
    null_bytes: usize,
    agg_state_offset: usize,
    num_keys: usize,
    num_rows: usize,
    current_block_idx: usize,
    current_row_in_block: usize,
    // Varchar arena (exposed to TaperColumnSerializeHandler via arena_alloc)
    varchar_arena_blocks: Vec<Vec<u8>>,
    varchar_arena_current_block: usize,
    varchar_arena_offset: usize,
}

impl RowContainer {
    /// Create with all fixed-width columns.
    pub fn new(key_sizes: &[usize], agg_state_size: usize) -> Self {
        let kinds = vec![ColumnKind::Fixed; key_sizes.len()];
        Self::with_kinds(key_sizes, &kinds, agg_state_size)
    }

    /// Create with explicit column kinds.
    pub fn with_kinds(key_sizes: &[usize], kinds: &[ColumnKind], agg_state_size: usize) -> Self {
        assert_eq!(key_sizes.len(), kinds.len());
        let num_keys = key_sizes.len();

        let mut offsets: Vec<usize> = Vec::with_capacity(num_keys);
        let mut cur_offset = 0usize;
        for i in 0..num_keys {
            offsets.push(cur_offset);
            cur_offset += match kinds[i] {
                ColumnKind::Fixed => key_sizes[i],
                ColumnKind::Varchar => VARCHAR_SLOT_SIZE,
            };
        }

        let null_block_start = cur_offset;
        let null_bytes = (num_keys + 7) / 8;
        let agg_state_offset = null_block_start + null_bytes;
        let row_size = agg_state_offset + agg_state_size;

        let columns: Vec<RowColumn> = (0..num_keys)
            .map(|i| RowColumn::pack(offsets[i], i, null_block_start))
            .collect();

        let first_block = vec![0u8; row_size * BLOCK_ROWS];
        let has_varchar = kinds.iter().any(|k| *k == ColumnKind::Varchar);
        let varchar_arena_blocks = if has_varchar { vec![vec![0u8; VARCHAR_ARENA_BLOCK_SIZE]] } else { Vec::new() };

        RowContainer {
            blocks: vec![first_block], row_size, columns, column_kinds: kinds.to_vec(),
            null_block_start, null_bytes, agg_state_offset, num_keys, num_rows: 0,
            current_block_idx: 0, current_row_in_block: 0,
            varchar_arena_blocks, varchar_arena_current_block: 0, varchar_arena_offset: 0,
        }
    }

    /// Mirrors C++ `RowContainer::NewRow()`.
    pub fn new_row(&mut self) -> *mut u8 {
        if self.current_row_in_block >= BLOCK_ROWS {
            let new_block = vec![0u8; self.row_size * BLOCK_ROWS];
            self.blocks.push(new_block);
            self.current_block_idx = self.blocks.len() - 1;
            self.current_row_in_block = 0;
        }
        let offset_in_block = self.current_row_in_block * self.row_size;
        self.current_row_in_block += 1;
        self.num_rows += 1;
        unsafe { self.blocks[self.current_block_idx].as_mut_ptr().add(offset_in_block) }
    }

    pub fn reserve(&mut self, additional: usize) {
        let rows_in_current = BLOCK_ROWS - self.current_row_in_block;
        if additional > rows_in_current {
            let extra_needed = additional - rows_in_current;
            let blocks_needed = (extra_needed + BLOCK_ROWS - 1) / BLOCK_ROWS;
            self.blocks.reserve(blocks_needed);
        }
    }

    /// Mirrors C++ `RowContainer::IsNullAt`.
    #[inline(always)]
    pub fn is_null_at(row: *const u8, null_byte: usize, null_mask: u8) -> bool {
        unsafe { *row.add(null_byte) & null_mask != 0 }
    }

    /// Mirrors C++ `RowContainer::SetNullAt`.
    #[inline(always)]
    pub fn set_null_at(row: *mut u8, null_byte: usize, null_mask: u8) {
        unsafe { *row.add(null_byte) |= null_mask; }
    }

    /// Mirrors C++ `RowContainer::ClearNullAt`.
    #[inline(always)]
    pub fn clear_null_at(row: *mut u8, null_byte: usize, null_mask: u8) {
        unsafe { *row.add(null_byte) &= !null_mask; }
    }

    /// Mirrors C++ `RowContainer::ReadValue<T>`.
    #[inline(always)]
    pub fn read_value<T: Copy>(row: *const u8, offset: usize) -> T {
        unsafe { (row.add(offset) as *const T).read_unaligned() }
    }

    /// Mirrors C++ `RowContainer::StoreValue<T>`.
    #[inline(always)]
    pub fn store_value<T: Copy>(row: *mut u8, offset: usize, value: T) {
        unsafe { (row.add(offset) as *mut T).write_unaligned(value); }
    }

    /// Mirrors C++ `RowContainer::ColumnAt`.
    #[inline(always)]
    pub fn column_at(&self, col_idx: usize) -> RowColumn { self.columns[col_idx] }

    /// Mirrors C++ `RowContainer::AggStateOffset`.
    #[inline(always)]
    pub fn agg_state_offset(&self) -> usize { self.agg_state_offset }

    /// Mirrors C++ `RowContainer::FixedRowSize`.
    #[inline(always)]
    pub fn row_size(&self) -> usize { self.row_size }

    /// Mirrors C++ `RowContainer::NumKeys`.
    pub fn num_keys(&self) -> usize { self.num_keys }

    /// Mirrors C++ `RowContainer::NumRows`.
    pub fn num_rows(&self) -> usize { self.num_rows }

    pub fn column_kind(&self, col_idx: usize) -> ColumnKind { self.column_kinds[col_idx] }

    // ─── Arena allocator (exposed for TaperColumnSerializeHandler) ───

    /// Allocate bytes from the varchar arena.
    /// Mirrors C++ `arenaAllocator.AllocateContinue(size, ...)`.
    pub fn arena_alloc(&mut self, size: usize) -> *mut u8 {
        if self.varchar_arena_blocks.is_empty()
            || self.varchar_arena_offset + size > VARCHAR_ARENA_BLOCK_SIZE
        {
            let block_size = size.max(VARCHAR_ARENA_BLOCK_SIZE);
            self.varchar_arena_blocks.push(vec![0u8; block_size]);
            self.varchar_arena_current_block = self.varchar_arena_blocks.len() - 1;
            self.varchar_arena_offset = 0;
        }
        let block = &mut self.varchar_arena_blocks[self.varchar_arena_current_block];
        let ptr = unsafe { block.as_mut_ptr().add(self.varchar_arena_offset) };
        self.varchar_arena_offset += size;
        ptr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_row_layout() {
        let rc = RowContainer::new(&[8, 4], 8);
        assert_eq!(rc.column_at(0).offset(), 0);
        assert_eq!(rc.column_at(1).offset(), 8);
        assert_eq!(rc.agg_state_offset(), 13);
        assert_eq!(rc.row_size(), 21);
        assert_eq!(rc.column_at(0).null_byte(), 12);
        assert_eq!(rc.column_at(0).null_mask(), 0x01);
        assert_eq!(rc.column_at(1).null_byte(), 12);
        assert_eq!(rc.column_at(1).null_mask(), 0x02);
    }

    #[test]
    fn test_row_container_basic() {
        let mut rc = RowContainer::new(&[8, 8], 8);
        let row = rc.new_row();
        let col0 = rc.column_at(0);
        let col1 = rc.column_at(1);
        RowContainer::store_value::<i64>(row, col0.offset(), 12345);
        RowContainer::store_value::<i64>(row, col1.offset(), 67890);
        assert_eq!(RowContainer::read_value::<i64>(row, col0.offset()), 12345);
        assert_eq!(RowContainer::read_value::<i64>(row, col1.offset()), 67890);
        assert!(!RowContainer::is_null_at(row, col0.null_byte(), col0.null_mask()));
    }

    #[test]
    fn test_null_operations() {
        let mut rc = RowContainer::new(&[8, 8], 8);
        let row = rc.new_row();
        let col0 = rc.column_at(0);
        assert!(!RowContainer::is_null_at(row, col0.null_byte(), col0.null_mask()));
        RowContainer::set_null_at(row, col0.null_byte(), col0.null_mask());
        assert!(RowContainer::is_null_at(row, col0.null_byte(), col0.null_mask()));
        RowContainer::clear_null_at(row, col0.null_byte(), col0.null_mask());
        assert!(!RowContainer::is_null_at(row, col0.null_byte(), col0.null_mask()));
    }

    #[test]
    fn test_many_columns_null_packing() {
        let rc = RowContainer::new(&[8; 10], 8);
        assert_eq!(rc.column_at(7).null_mask(), 0x80);
        let null_block_start = 8 * 10;
        assert_eq!(rc.column_at(8).null_byte(), null_block_start + 1);
        assert_eq!(rc.column_at(8).null_mask(), 0x01);
        assert_eq!(rc.column_at(9).null_byte(), null_block_start + 1);
        assert_eq!(rc.column_at(9).null_mask(), 0x02);
    }

    #[test]
    fn test_pointer_stability() {
        let mut rc = RowContainer::new(&[8, 8], 8);
        let mut ptrs: Vec<*mut u8> = Vec::new();
        let col0 = rc.column_at(0);
        for i in 0..5000 {
            let row = rc.new_row();
            RowContainer::store_value::<i64>(row, col0.offset(), i as i64);
            ptrs.push(row);
        }
        for (i, &ptr) in ptrs.iter().enumerate() {
            assert_eq!(RowContainer::read_value::<i64>(ptr, col0.offset()), i as i64);
        }
        assert_eq!(rc.num_rows(), 5000);
    }
}
