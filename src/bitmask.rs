/// BitMask: encodes which slots in a chunk matched.
/// Each matching slot has its corresponding byte's MSB set in the mask.
#[derive(Clone, Copy)]
pub struct BitMask(pub u64);

const MSBS: u64 = 0x8080808080808080;
const LSBS: u64 = 0x0101010101010101;

impl BitMask {
    /// SWAR tag match: find all slots whose tag == target.
    /// Works on all platforms, no SIMD instructions needed.
    #[inline(always)]
    pub fn match_tag(tags: u64, target: u8) -> Self {
        let x = tags ^ LSBS.wrapping_mul(target as u64);
        BitMask(x.wrapping_sub(LSBS) & !x & MSBS)
    }

    /// Find all empty slots (tag == 0x80).
    #[inline(always)]
    pub fn match_empty(tags: u64) -> Self {
        BitMask((tags & (!tags << 7)) & MSBS)
    }

    /// Are there any matches?
    #[inline(always)]
    pub fn any(self) -> bool {
        self.0 != 0
    }

    /// Number of matches.
    #[inline(always)]
    pub fn count(self) -> u32 {
        (self.0 >> 7).count_ones()  // each match has bit7 set
    }

    /// Get the lowest matching slot index (0~7).
    #[inline(always)]
    pub fn lowest(self) -> u8 {
        (self.0.trailing_zeros() >> 3) as u8
    }

    /// Remove the lowest match and return remaining.
    #[inline(always)]
    pub fn advance(self) -> Self {
        BitMask(self.0 & (self.0 - 1))
    }
}

/// Iterator over matching slot indices.
impl Iterator for BitMask {
    type Item = u8;

    #[inline(always)]
    fn next(&mut self) -> Option<u8> {
        if self.0 == 0 {
            return None;
        }
        let slot = self.lowest();
        *self = self.advance();
        Some(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_match_tag_basic() {
        // Actually build it byte by byte for clarity:
        let tags = u64::from_le_bytes([0x2A, 0x3F, 0x2A, 0x80, 0x80, 0x80, 0x80, 0x80]);
        let mask = BitMask::match_tag(tags, 0x2A);
        let matches: Vec<u8> = mask.collect();
        assert_eq!(matches, vec![0, 2]);
    }

    #[test]
    fn test_match_empty() {
        let tags = u64::from_le_bytes([0x2A, 0x80, 0x3F, 0x80, 0x80, 0x80, 0x80, 0x80]);
        let mask = BitMask::match_empty(tags);
        let empties: Vec<u8> = mask.collect();
        assert_eq!(empties, vec![1, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn test_no_match() {
        let tags = u64::from_le_bytes([0x2A, 0x3F, 0x11, 0x80, 0x80, 0x80, 0x80, 0x80]);
        let mask = BitMask::match_tag(tags, 0x55);
        assert!(!mask.any());
        assert_eq!(mask.count(), 0);
    }
}
