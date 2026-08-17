use crate::bitmask::BitMask;

const SLOTS_PER_CHUNK: usize = 8;
const EMPTY_TAG: u8 = 0x80;

/// A 128-byte aligned chunk holding 8 slots.
/// Layout matches C++ TaperHashTableChunk (for Key=u64, Value=6B):
///   [tags: 8B @ 0][keys: 64B @ 8][pad: 8B @ 72][values: 48B @ 80]
/// Total = 128B = 2 cache lines.
///
/// C++ offset calculation:
///   keyOffsetInChunk_ = (elemNum + 7) & 0xF8 = (8+7)&0xF8 = 8
///   valOffsetInChunk_ = (keyOffset + elemNum*keySize + 15) & 0xF0 = (8+64+15)&0xF0 = 80
#[repr(C, align(128))]
pub struct Chunk {
    pub tags: [u8; SLOTS_PER_CHUNK],          // offset 0, 8B
    pub keys: [u64; SLOTS_PER_CHUNK],          // offset 8, 64B
    _key_val_pad: [u8; 8],                     // offset 72, 8B (align values to 16B boundary)
    pub values: [SlotValue; SLOTS_PER_CHUNK],  // offset 80, 48B
}

/// 6-byte compressed pointer stored in each slot's value area.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SlotValue {
    pub bytes: [u8; 6],
}

impl SlotValue {
    /// Store a pointer (lower 48 bits).
    #[inline(always)]
    pub fn set_ptr(&mut self, ptr: *const u8) {
        let val = ptr as u64;
        self.bytes.copy_from_slice(&val.to_le_bytes()[..6]);
    }

    /// Read back the pointer.
    #[inline(always)]
    pub fn get_ptr(&self) -> *const u8 {
        let mut buf = [0u8; 8];
        buf[..6].copy_from_slice(&self.bytes);
        u64::from_le_bytes(buf) as *const u8
    }

    /// Read back as mutable pointer.
    #[inline(always)]
    pub fn get_ptr_mut(&self) -> *mut u8 {
        self.get_ptr() as *mut u8
    }
}

impl Chunk {
    /// Create an empty chunk (all tags = 0x80).
    pub fn new() -> Self {
        Chunk {
            tags: [EMPTY_TAG; SLOTS_PER_CHUNK],
            keys: [0; SLOTS_PER_CHUNK],
            _key_val_pad: [0; 8],
            values: [SlotValue::default(); SLOTS_PER_CHUNK],
        }
    }

    /// Load all 8 tags as a single u64 for SWAR comparison.
    #[inline(always)]
    pub fn tags_u64(&self) -> u64 {
        // Static assert: Chunk must be exactly 128 bytes (matching C++ static_assert)
        const _: () = assert!(std::mem::size_of::<Chunk>() == 128);
        u64::from_le_bytes(self.tags)
    }

    /// Try to emplace at this chunk position.
    /// Returns true if successful (found match or empty slot).
    /// Returns false if chunk is full (need to probe next chunk).
    #[inline]
    pub fn try_emplace<FKeyCmp, FInit, FUpdate>(
        &mut self,
        key: u64,
        hash_val: u64,
        key_cmp: &FKeyCmp,
        on_init: &mut FInit,
        on_update: &mut FUpdate,
    ) -> bool
    where
        FKeyCmp: Fn(u64, u64) -> bool,  // (input_key, slot_key) -> equal?
        FInit: FnMut(&mut SlotValue),
        FUpdate: FnMut(&SlotValue, bool),  // (slot_value, is_new)
    {
        let tag_hash = ((hash_val >> 16) & 0x7F) as u8;
        let tags = self.tags_u64();

        // Stage 1a+1b: find tag match, then compare key (hash int64)
        for i in BitMask::match_tag(tags, tag_hash) {
            let slot = i as usize;
            if key_cmp(key, self.keys[slot]) {
                // Key matches → existing group
                on_update(&self.values[slot], false);
                return true;
            }
        }

        // Find empty slot → new group
        for i in BitMask::match_empty(tags) {
            let slot = i as usize;
            self.tags[slot] = tag_hash;
            self.keys[slot] = key;
            on_init(&mut self.values[slot]);
            on_update(&self.values[slot], true);
            return true;
        }

        false  // Chunk full
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_new_is_empty() {
        let chunk = Chunk::new();
        assert!(chunk.tags.iter().all(|&t| t == EMPTY_TAG));
    }

    #[test]
    fn test_slot_value_ptr_roundtrip() {
        let mut sv = SlotValue::default();
        let data: u64 = 0x0000_AABB_CCDD_1122;
        sv.set_ptr(data as *const u8);
        assert_eq!(sv.get_ptr() as u64, data);
    }

    #[test]
    fn test_try_emplace_new_and_existing() {
        let mut chunk = Chunk::new();
        let mut init_called = false;
        let mut update_is_new = None;

        // First insert: should find empty slot
        let ok = chunk.try_emplace(
            42, 42,
            &|a, b| a == b,
            &mut |_slot| { init_called = true; },
            &mut |_slot, is_new| { update_is_new = Some(is_new); },
        );
        assert!(ok);
        assert!(init_called);
        assert_eq!(update_is_new, Some(true));

        // Second insert with same key: should find existing
        init_called = false;
        update_is_new = None;
        let ok = chunk.try_emplace(
            42, 42,
            &|a, b| a == b,
            &mut |_slot| { init_called = true; },
            &mut |_slot, is_new| { update_is_new = Some(is_new); },
        );
        assert!(ok);
        assert!(!init_called);
        assert_eq!(update_is_new, Some(false));
    }
}
