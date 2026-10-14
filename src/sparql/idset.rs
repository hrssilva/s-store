//! A set of term ids for semi-, anti- and outer joins on one variable.
//!
//! Dictionary ids are dense (below the dictionary's id bound), so the set is
//! one bit per id: no hashing, no stored keys. The words are allocated
//! zeroed, which the allocator serves from untouched zero pages, so a sparse
//! set only costs the pages it writes. Clearing resets just the words it
//! touched (or everything, once that list would grow past a fraction of the
//! bitmap), so a pooled set is cheap to reuse across evaluations.
//! Query-local ids (computed terms, above the dictionary's ids) go to a
//! small sorted side list.

/// Above this fraction of touched words, `clear` resets the whole bitmap
/// instead of remembering every touched word.
const TOUCHED_FRACTION: usize = 16;

pub struct IdSet {
    words: Vec<u64>,
    /// Indices of words that went from zero to non-zero, while there are
    /// fewer than `words.len() / TOUCHED_FRACTION` of them.
    touched: Vec<u32>,
    /// More words were touched than `touched` records.
    dense: bool,
    /// Ids beyond the bitmap; sorted and deduplicated by [`Self::freeze`].
    others: Vec<u32>,
}

impl IdSet {
    /// A set over ids `0..bound` (others are kept aside).
    pub fn new(bound: u32) -> Self {
        Self { words: vec![0; (bound as usize).div_ceil(64)], touched: Vec::new(), dense: false, others: Vec::new() }
    }

    /// Whether the bitmap covers ids `0..bound`.
    pub fn covers(&self, bound: u32) -> bool {
        self.words.len() == (bound as usize).div_ceil(64)
    }

    pub fn insert(&mut self, id: u32) {
        let w = (id / 64) as usize;
        let limit = self.words.len() / TOUCHED_FRACTION;
        let Some(word) = self.words.get_mut(w) else {
            self.others.push(id);
            return;
        };
        if *word == 0 && !self.dense {
            if self.touched.len() < limit {
                self.touched.push(w as u32);
            } else {
                self.dense = true;
                self.touched = Vec::new();
            }
        }
        *word |= 1 << (id % 64);
    }

    /// Makes ids inserted beyond the bitmap searchable; call after the last
    /// insert and before `contains`.
    pub fn freeze(&mut self) {
        self.others.sort_unstable();
        self.others.dedup();
    }

    pub fn contains(&self, id: u32) -> bool {
        match self.words.get((id / 64) as usize) {
            Some(word) => word & (1 << (id % 64)) != 0,
            None => self.others.binary_search(&id).is_ok(),
        }
    }

    pub fn clear(&mut self) {
        if self.dense {
            self.words.fill(0);
            self.dense = false;
        } else {
            for &w in &self.touched {
                self.words[w as usize] = 0;
            }
        }
        self.touched.clear();
        self.others.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_and_side_list_membership() {
        let mut set = IdSet::new(200);
        for id in [0, 63, 64, 199, 5000, 0xF000_0001, 5000] {
            set.insert(id);
        }
        set.freeze();
        for id in [0, 63, 64, 199, 5000, 0xF000_0001] {
            assert!(set.contains(id), "{id}");
        }
        for id in [1, 65, 198, 200, 4999, 0xF000_0000] {
            assert!(!set.contains(id), "{id}");
        }
    }

    #[test]
    fn clearing_resets_touched_words_or_everything() {
        let mut sparse = IdSet::new(64 * 1024);
        sparse.insert(5);
        sparse.insert(64 * 1000);
        sparse.clear();
        assert!(!sparse.contains(5) && !sparse.contains(64 * 1000));

        let mut dense = IdSet::new(64 * 32);
        (0..64 * 32).step_by(64).for_each(|id| dense.insert(id));
        assert!(dense.dense, "more touched words than worth remembering");
        dense.insert(7_000_000);
        dense.freeze();
        dense.clear();
        assert!((0..64 * 32).all(|id| !dense.contains(id)) && !dense.contains(7_000_000));
        dense.insert(3);
        assert!(dense.contains(3) && !dense.dense);
    }
}
