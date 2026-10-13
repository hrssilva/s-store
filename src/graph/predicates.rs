//! Dense predicate ids.
//!
//! In the dictionary's id space a predicate coordinate would span every term
//! id, widening the tree's predicate dimension to the subject/object width.
//! Predicates are few, so they get a dense id space of their own (as in
//! s-bench's graph stage): a `Vec` from dense id to term, and a sorted
//! `Vec` from term to dense id. Both are a handful of bytes per distinct
//! predicate; no hashing structure is kept.
//!
//! A dense id is never reused, so trees and query plans can hold one without
//! tracking predicate removal.

use s_dict::TermId;

#[derive(Clone, Debug, Default)]
pub struct PredicateTable {
    terms: Vec<TermId>,
    /// `(term, dense id)`, sorted by term.
    by_term: Vec<(TermId, u32)>,
}

impl PredicateTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, term: TermId) -> Option<u32> {
        self.by_term.binary_search_by_key(&term, |&(t, _)| t).ok().map(|i| self.by_term[i].1)
    }

    pub fn intern(&mut self, term: TermId) -> u32 {
        match self.by_term.binary_search_by_key(&term, |&(t, _)| t) {
            Ok(i) => self.by_term[i].1,
            Err(i) => {
                let dense = self.terms.len() as u32;
                self.terms.push(term);
                self.by_term.insert(i, (term, dense));
                dense
            }
        }
    }

    /// The term of a dense id. Panics on an id this table never issued.
    pub fn term(&self, dense: u32) -> TermId {
        self.terms[dense as usize]
    }

    pub fn len(&self) -> usize {
        self.terms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// The largest dense id issued (0 when empty).
    pub fn max_id(&self) -> u32 {
        self.terms.len().saturating_sub(1) as u32
    }

    /// Every predicate term with its dense id, in dense id order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (u32, TermId)> + '_ {
        self.terms.iter().enumerate().map(|(i, &t)| (i as u32, t))
    }

    pub fn size_bytes(&self) -> usize {
        self.terms.capacity() * size_of::<TermId>() + self.by_term.capacity() * size_of::<(TermId, u32)>()
    }

    /// Rebuilds a table from its dense-order term list (persistence).
    pub fn from_terms(terms: Vec<TermId>) -> Self {
        let mut by_term: Vec<(TermId, u32)> = terms.iter().enumerate().map(|(i, &t)| (t, i as u32)).collect();
        by_term.sort_unstable();
        Self { terms, by_term }
    }

    pub fn terms(&self) -> &[TermId] {
        &self.terms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dense_ids_follow_first_use_and_are_stable() {
        let mut t = PredicateTable::new();
        assert_eq!(t.intern(TermId(900)), 0);
        assert_eq!(t.intern(TermId(5)), 1);
        assert_eq!(t.intern(TermId(900)), 0);
        assert_eq!(t.intern(TermId(70)), 2);
        assert_eq!(t.len(), 3);
        assert_eq!(t.max_id(), 2);
        assert_eq!(t.get(TermId(5)), Some(1));
        assert_eq!(t.get(TermId(6)), None);
        assert_eq!(t.term(2), TermId(70));
        assert_eq!(t.iter().collect::<Vec<_>>(), vec![(0, TermId(900)), (1, TermId(5)), (2, TermId(70))]);
    }

    #[test]
    fn rebuilds_from_terms() {
        let t = PredicateTable::from_terms(vec![TermId(9), TermId(3)]);
        assert_eq!(t.get(TermId(3)), Some(1));
        assert_eq!(t.get(TermId(9)), Some(0));
    }
}
