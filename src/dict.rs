//! The term dictionary: canonical keys ([`crate::term`]) to `u32` ids.
//!
//! A thin layer over [`s_dict::PackedAutomatonDictionary`] that adds what
//! the store needs and the dictionary deliberately leaves out:
//!
//! - **An id ceiling.** Ids from [`LOCAL_ID_BASE`] up are reserved for terms
//!   a query computes (`BIND`, aggregates) that are not in the dictionary, so
//!   solution rows stay plain `u32` ids end to end.
//! - **The largest id issued**, which sizes the graph trees' domains.
//!
//! Terms are never removed when quads are deleted: s-dict reuses freed ids,
//! and a term may still be referenced from another graph or a property.
//! Garbage terms are reclaimable by a future compaction pass.
//!
//! The query engine depends on the [`Terms`] trait, not on this type.

use crate::error::Result;
use s_dict::{PackedAutomatonDictionary, PreparedBatch, TermDictionary, TermId};

/// First id reserved for query-computed terms. The dictionary holds up to
/// 3.75 billion terms; each query may compute up to 268 million more.
pub const LOCAL_ID_BASE: u32 = 0xF000_0000;

/// Id-to-key and key-to-id lookups, as the query engine needs them.
pub trait Terms {
    fn id_of(&self, key: &str) -> Option<TermId>;
    /// Writes `id`'s key into `buf` (cleared first); `false` if unknown.
    fn key_into(&self, id: TermId, buf: &mut Vec<u8>) -> bool;
}

pub struct Dictionary {
    inner: PackedAutomatonDictionary,
    /// One past the largest id ever issued (0 when none).
    id_bound: u32,
}

impl Default for Dictionary {
    fn default() -> Self {
        Self::new()
    }
}

impl Dictionary {
    pub fn new() -> Self {
        Self::from_parts(PackedAutomatonDictionary::new(), 0)
    }

    /// Wraps a loaded dictionary. `id_bound` is the value [`Self::id_bound`]
    /// had when it was saved: s-dict does not expose its next id, and
    /// recomputing the bound would mean listing every id.
    pub fn from_parts(mut inner: PackedAutomatonDictionary, id_bound: u32) -> Self {
        inner.set_id_limit(LOCAL_ID_BASE);
        Self { inner, id_bound }
    }

    pub fn lookup(&self, key: &str) -> Option<TermId> {
        self.inner.get_id(key)
    }

    pub fn intern(&mut self, key: &str) -> Result<TermId> {
        let id = self.inner.try_insert(key)?;
        self.id_bound = self.id_bound.max(id.0 + 1);
        Ok(id)
    }

    /// Interns a prepared batch; ids come back in input order.
    pub fn intern_prepared(&mut self, batch: &PreparedBatch) -> Result<Vec<TermId>> {
        let ids = self.inner.try_insert_prepared(batch)?;
        if let Some(max) = ids.iter().map(|id| id.0).max() {
            self.id_bound = self.id_bound.max(max + 1);
        }
        Ok(ids)
    }

    pub fn key_into(&self, id: TermId, buf: &mut Vec<u8>) -> bool {
        self.inner.get_term_into(id, buf)
    }

    /// The key of `id` as an owned string (convenience; hot paths use
    /// [`Self::key_into`] with a reused buffer).
    pub fn key(&self, id: TermId) -> Option<String> {
        self.inner.get_term(id)
    }

    /// One past the largest id issued.
    pub fn id_bound(&self) -> u32 {
        self.id_bound
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// End of a load: compacts the dictionary (see s-dict's `shrink_to_fit`).
    pub fn shrink_to_fit(&mut self) {
        self.inner.shrink_to_fit();
    }

    pub fn size_bytes(&self) -> usize {
        self.inner.size_bytes()
    }

    pub fn inner(&self) -> &PackedAutomatonDictionary {
        &self.inner
    }
}

impl Terms for Dictionary {
    fn id_of(&self, key: &str) -> Option<TermId> {
        self.lookup(key)
    }

    fn key_into(&self, id: TermId, buf: &mut Vec<u8>) -> bool {
        Dictionary::key_into(self, id, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use s_dict::TermBatch;

    #[test]
    fn interns_and_resolves_both_ways() {
        let mut d = Dictionary::new();
        let a = d.intern("<a>").unwrap();
        assert_eq!(d.intern("<a>").unwrap(), a);
        let b = d.intern("\"b\"").unwrap();
        assert_ne!(a, b);
        assert_eq!(d.lookup("<a>"), Some(a));
        assert_eq!(d.lookup("<zz>"), None);
        let mut buf = Vec::new();
        assert!(d.key_into(b, &mut buf));
        assert_eq!(buf, b"\"b\"");
        assert_eq!(d.key(a).as_deref(), Some("<a>"));
        assert_eq!(d.len(), 2);
        assert_eq!(d.id_bound(), a.0.max(b.0) + 1);
    }

    #[test]
    fn prepared_batches_keep_input_order() {
        let mut d = Dictionary::new();
        let mut batch = TermBatch::new();
        for k in ["<x>", "<y>", "<x>"] {
            batch.push(k);
        }
        let ids = d.intern_prepared(&batch.prepare()).unwrap();
        assert_eq!(ids[0], ids[2]);
        assert_ne!(ids[0], ids[1]);
        assert_eq!(d.lookup("<y>"), Some(ids[1]));
        assert!(d.id_bound() > ids[0].0.max(ids[1].0));
    }

    #[test]
    fn ids_stay_below_the_local_base() {
        let mut d = Dictionary::new();
        assert!(d.intern("<a>").unwrap().0 < LOCAL_ID_BASE);
        assert_eq!(d.inner().ids_available(), u64::from(LOCAL_ID_BASE) - 1);
    }

    #[test]
    fn id_bound_survives_reload() {
        let mut d = Dictionary::new();
        for k in ["<a>", "<b>", "<c>"] {
            d.intern(k).unwrap();
        }
        let mut bytes = Vec::new();
        d.inner().save(&mut bytes).unwrap();
        let loaded = Dictionary::from_parts(s_dict::PackedAutomatonDictionary::load(&bytes).unwrap(), d.id_bound());
        assert_eq!(loaded.id_bound(), d.id_bound());
        assert_eq!(loaded.lookup("<b>"), d.lookup("<b>"));
    }
}
