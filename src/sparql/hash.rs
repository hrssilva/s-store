//! An index from hashed keys to entry numbers whose keys live elsewhere.
//!
//! DISTINCT, GROUP BY and query-local term interning all need "have I seen
//! this key, and under which number?" for keys that already sit in a flat
//! buffer (a row of ids, a span of text). Keying a `HashMap` by them would
//! copy every key into its own allocation; this table stores only
//! `(hash, entry)` pairs and asks the caller to compare an entry's key, so
//! keys are stored once, in the caller's buffer.

/// FxHash-style word hashing: fast and adequate for in-memory tables.
#[derive(Clone, Copy, Default)]
pub struct Hasher(u64);

impl Hasher {
    const K: u64 = 0x517c_c1b7_2722_0a95;

    #[inline]
    pub fn word(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(Self::K);
    }

    pub fn bytes(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for &w in words {
            self.word(u64::from_le_bytes(w));
        }
        let mut last = [0u8; 8];
        last[..rest.len()].copy_from_slice(rest);
        self.word(u64::from_le_bytes(last) ^ ((bytes.len() as u64) << 56));
    }

    pub fn finish(self) -> u64 {
        // Mix high bits down: the table indexes with the low bits.
        self.0 ^ (self.0 >> 29)
    }
}

pub fn hash_ids(ids: &[u32]) -> u64 {
    let mut h = Hasher::default();
    for pair in ids.chunks(2) {
        h.word(u64::from(pair[0]) | (u64::from(*pair.get(1).unwrap_or(&0)) << 32));
    }
    h.word(ids.len() as u64);
    h.finish()
}

pub fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = Hasher::default();
    h.bytes(bytes);
    h.finish()
}

const EMPTY: u32 = u32::MAX;

/// Open addressing, linear probing, power-of-two capacity.
#[derive(Default)]
pub struct RawIndex {
    slots: Vec<(u64, u32)>,
    len: usize,
}

impl RawIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.slots.iter_mut().for_each(|s| *s = (0, EMPTY));
        self.len = 0;
    }

    /// The entry whose key `eq` accepts, if any.
    pub fn find(&self, hash: u64, mut eq: impl FnMut(u32) -> bool) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut i = hash as usize & mask;
        loop {
            let (h, e) = self.slots[i];
            if e == EMPTY {
                return None;
            }
            if h == hash && eq(e) {
                return Some(e);
            }
            i = (i + 1) & mask;
        }
    }

    /// The existing entry `eq` accepts, or `new_entry` inserted:
    /// `(entry, inserted)`.
    pub fn find_or_insert(&mut self, hash: u64, eq: impl FnMut(u32) -> bool, new_entry: u32) -> (u32, bool) {
        if let Some(e) = self.find(hash, eq) {
            return (e, false);
        }
        if (self.len + 1) * 8 > self.slots.len() * 7 {
            self.grow();
        }
        let mask = self.slots.len() - 1;
        let mut i = hash as usize & mask;
        while self.slots[i].1 != EMPTY {
            i = (i + 1) & mask;
        }
        self.slots[i] = (hash, new_entry);
        self.len += 1;
        (new_entry, true)
    }

    fn grow(&mut self) {
        let capacity = (self.slots.len() * 2).max(16);
        let old = std::mem::replace(&mut self.slots, vec![(0, EMPTY); capacity]);
        let mask = capacity - 1;
        for (h, e) in old.into_iter().filter(|&(_, e)| e != EMPTY) {
            let mut i = h as usize & mask;
            while self.slots[i].1 != EMPTY {
                i = (i + 1) & mask;
            }
            self.slots[i] = (h, e);
        }
    }
}

/// A set of fixed-width id rows, stored back to back in one buffer.
#[derive(Default)]
pub struct RowSet {
    width: usize,
    rows: Vec<u32>,
    index: RawIndex,
}

impl RowSet {
    pub fn new(width: usize) -> Self {
        Self { width, rows: Vec::new(), index: RawIndex::new() }
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The row's entry number, and whether it was new.
    pub fn insert(&mut self, row: &[u32]) -> (u32, bool) {
        debug_assert_eq!(row.len(), self.width);
        let w = self.width;
        let next = self.index.len() as u32;
        let rows = &self.rows;
        let (entry, inserted) = self.index.find_or_insert(hash_ids(row), |e| &rows[e as usize * w..(e as usize + 1) * w] == row, next);
        if inserted {
            self.rows.extend_from_slice(row);
        }
        (entry, inserted)
    }

    pub fn row(&self, entry: u32) -> &[u32] {
        &self.rows[entry as usize * self.width..(entry as usize + 1) * self.width]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_deduplicated_and_numbered_in_insertion_order() {
        let mut set = RowSet::new(2);
        assert_eq!(set.insert(&[1, 2]), (0, true));
        assert_eq!(set.insert(&[2, 1]), (1, true));
        assert_eq!(set.insert(&[1, 2]), (0, false));
        for i in 0..1000u32 {
            set.insert(&[i, i + 7]);
        }
        assert_eq!(set.len(), 1002);
        assert_eq!(set.insert(&[500, 507]), (502, false));
        assert_eq!(set.row(1), &[2, 1]);
    }

    #[test]
    fn colliding_hashes_are_told_apart_by_key() {
        let mut idx = RawIndex::new();
        let keys = ["a", "b", "c"];
        for (i, _) in keys.iter().enumerate() {
            idx.find_or_insert(42, |e| keys[e as usize] == keys[i], i as u32);
        }
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.find(42, |e| keys[e as usize] == "b"), Some(1));
        assert_eq!(idx.find(42, |e| keys[e as usize] == "z"), None);
    }

    #[test]
    fn byte_hashing_depends_on_length_and_content() {
        assert_ne!(hash_bytes(b"abc"), hash_bytes(b"abc\0"));
        assert_ne!(hash_bytes(b"abcdefgh1"), hash_bytes(b"abcdefgh2"));
        assert_eq!(hash_bytes(b"same"), hash_bytes(b"same"));
        assert_ne!(hash_ids(&[1]), hash_ids(&[1, 0]));
    }
}
