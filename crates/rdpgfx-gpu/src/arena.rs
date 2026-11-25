//! First-fit range allocator for sub-allocating large GPU buffers (bitmap cache).
//!
//! Vulkan limits the number of memory allocations (4096 on NVIDIA), while RDPGFX allows
//! thousands of cache slots, so cache entries live in a few large chunks.

/// Free-list allocator over `0..capacity`.
#[derive(Debug, Clone)]
pub struct Arena {
    capacity: u64,
    /// Free ranges `(offset, len)`, sorted by offset, never adjacent (always merged).
    free: Vec<(u64, u64)>,
}

/// Allocation granularity; also satisfies the 4-byte texel alignment of buffer/image copies.
pub const ALIGN: u64 = 256;

const fn round_up(v: u64) -> u64 {
    v.div_ceil(ALIGN) * ALIGN
}

impl Arena {
    pub fn new(capacity: u64) -> Self {
        Self {
            capacity,
            free: vec![(0, capacity)],
        }
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    pub fn free_bytes(&self) -> u64 {
        self.free.iter().map(|f| f.1).sum()
    }

    /// Size actually reserved for a request of `size` bytes.
    pub const fn reserved(size: u64) -> u64 {
        round_up(if size == 0 { 1 } else { size })
    }

    /// Returns the offset of a block of at least `size` bytes, or `None` if no free range fits.
    pub fn alloc(&mut self, size: u64) -> Option<u64> {
        let need = Self::reserved(size);
        let i = self.free.iter().position(|f| f.1 >= need)?;
        let (off, len) = self.free[i];
        if len == need {
            self.free.remove(i);
        } else {
            self.free[i] = (off + need, len - need);
        }
        Some(off)
    }

    /// Releases a block returned by `alloc` with the same `size`.
    pub fn release(&mut self, offset: u64, size: u64) {
        let len = Self::reserved(size);
        debug_assert!(offset + len <= self.capacity);
        let i = self.free.partition_point(|f| f.0 < offset);
        debug_assert!(i == 0 || self.free[i - 1].0 + self.free[i - 1].1 <= offset);
        debug_assert!(i == self.free.len() || offset + len <= self.free[i].0);
        self.free.insert(i, (offset, len));
        // Merge with the following, then with the preceding range.
        if i + 1 < self.free.len() && self.free[i].0 + self.free[i].1 == self.free[i + 1].0 {
            self.free[i].1 += self.free[i + 1].1;
            self.free.remove(i + 1);
        }
        if i > 0 && self.free[i - 1].0 + self.free[i - 1].1 == self.free[i].0 {
            self.free[i - 1].1 += self.free[i].1;
            self.free.remove(i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_release_merges_back_to_one_range() {
        let mut a = Arena::new(4096);
        let x = a.alloc(100).unwrap();
        let y = a.alloc(300).unwrap();
        let z = a.alloc(256).unwrap();
        assert_eq!((x, y, z), (0, 256, 768));
        assert_eq!(a.free_bytes(), 4096 - 1024);
        a.release(y, 300);
        // A smaller block reuses the hole first (first fit).
        assert_eq!(a.alloc(10), Some(256));
        a.release(256, 10);
        a.release(x, 100);
        a.release(z, 256);
        assert_eq!(a.free, vec![(0, 4096)]);
    }

    #[test]
    fn exhaustion_and_exact_fit() {
        let mut a = Arena::new(1024);
        let blocks: Vec<u64> = (0..4).map(|_| a.alloc(256).unwrap()).collect();
        assert_eq!(a.alloc(1), None);
        a.release(blocks[2], 256);
        assert_eq!(a.alloc(257), None);
        assert_eq!(a.alloc(256), Some(512));
    }

    #[test]
    fn random_sequence_keeps_invariants() {
        let mut a = Arena::new(1 << 20);
        let mut live: Vec<(u64, u64)> = Vec::new();
        let mut s = 0x1234_5678_u64;
        for _ in 0..5000 {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if live.is_empty() || !(s >> 40).is_multiple_of(3) {
                let size = (s >> 20) % 20000 + 1;
                if let Some(off) = a.alloc(size) {
                    live.push((off, size));
                }
            } else {
                let i = ((s >> 33) as usize) % live.len();
                let (off, size) = live.swap_remove(i);
                a.release(off, size);
            }
            // No overlaps between live blocks, accounting adds up.
            let mut sorted: Vec<_> = live.iter().map(|&(o, s)| (o, Arena::reserved(s))).collect();
            sorted.sort();
            for w in sorted.windows(2) {
                assert!(w[0].0 + w[0].1 <= w[1].0);
            }
            let used: u64 = sorted.iter().map(|b| b.1).sum();
            assert_eq!(used + a.free_bytes(), a.capacity());
        }
        for (off, size) in live {
            a.release(off, size);
        }
        assert_eq!(a.free, vec![(0, 1 << 20)]);
    }
}
