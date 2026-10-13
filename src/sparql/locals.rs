//! Terms a query computes that the dictionary does not hold.
//!
//! `BIND(?x + 1 AS ?y)`, aggregates, CONSTRUCT template blank nodes and query
//! constants missing from the data all produce terms without a dictionary
//! id. They get ids from [`LOCAL_ID_BASE`] up, so solution rows stay plain
//! `u32`s. A computed term that *is* in the dictionary always gets its
//! dictionary id instead (see [`LocalTerms::id_for`]), so id equality stays
//! term equality across both id spaces.
//!
//! Keys are stored back to back in one string and indexed by
//! [`RawIndex`]; interning allocates nothing per term beyond buffer growth.

use super::hash::{RawIndex, hash_bytes};
use crate::dict::{LOCAL_ID_BASE, Terms};

#[derive(Default, Debug)]
pub struct LocalTerms {
    text: String,
    ends: Vec<u32>,
    index: RawIndexDebug,
}

#[derive(Default)]
struct RawIndexDebug(RawIndex);

impl std::fmt::Debug for RawIndexDebug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RawIndex({})", self.0.len())
    }
}

impl LocalTerms {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    fn key_of(&self, local: u32) -> &str {
        let end = self.ends[local as usize] as usize;
        let start = if local == 0 { 0 } else { self.ends[local as usize - 1] as usize };
        &self.text[start..end]
    }

    /// The id of the term with canonical `key`: its dictionary id if the
    /// dictionary has it, otherwise a local id.
    pub fn id_for(&mut self, terms: &impl Terms, key: &str) -> u32 {
        match terms.id_of(key) {
            Some(id) => id.0,
            None => self.intern(key),
        }
    }

    /// A local id for `key`, which the caller knows is not a dictionary key.
    pub fn intern(&mut self, key: &str) -> u32 {
        let next = self.ends.len() as u32;
        let (text, ends) = (&self.text, &self.ends);
        let key_at = |local: u32| {
            let end = ends[local as usize] as usize;
            let start = if local == 0 { 0 } else { ends[local as usize - 1] as usize };
            &text[start..end]
        };
        let (local, inserted) = self.index.0.find_or_insert(hash_bytes(key.as_bytes()), |e| key_at(e) == key, next);
        if inserted {
            self.text.push_str(key);
            self.ends.push(self.text.len() as u32);
        }
        assert!(local < u32::MAX - LOCAL_ID_BASE - 1, "query-local term space exhausted");
        LOCAL_ID_BASE + local
    }

    /// The key of a local id; `None` for a dictionary id.
    pub fn key(&self, id: u32) -> Option<&str> {
        let local = id.checked_sub(LOCAL_ID_BASE)?;
        ((local as usize) < self.ends.len()).then(|| self.key_of(local))
    }

    pub fn is_local(id: u32) -> bool {
        id >= LOCAL_ID_BASE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::Dictionary;

    #[test]
    fn dictionary_terms_keep_their_ids() {
        let mut dict = Dictionary::new();
        let a = dict.intern("<a>").unwrap();
        let mut locals = LocalTerms::new();
        assert_eq!(locals.id_for(&dict, "<a>"), a.0);
        let b = locals.id_for(&dict, "<b>");
        assert!(LocalTerms::is_local(b));
        assert_eq!(locals.id_for(&dict, "<b>"), b);
        assert_eq!(locals.key(b), Some("<b>"));
        assert_eq!(locals.key(a.0), None);
        assert_eq!(locals.len(), 1);
    }

    #[test]
    fn many_local_terms() {
        let mut locals = LocalTerms::new();
        let ids: Vec<u32> = (0..500).map(|i| locals.intern(&format!("\"{i}\""))).collect();
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(locals.key(id), Some(format!("\"{i}\"").as_str()));
            assert_eq!(locals.intern(&format!("\"{i}\"")), id);
        }
    }
}
