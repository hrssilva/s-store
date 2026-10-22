//! Coordinate domains for a partition's FST.
//!
//! s-graph fixes an FST's per-dimension domain sizes at construction, and its
//! dynamic schedule spends 6 bits (one 64-way level) per trie level across
//! S, P and O. A total bit width that is not a multiple of 6 leaves a partial
//! bottom level and, as measured in s-bench (`graph_stage::aligned_sizes`),
//! can multiply a radix tree's size several times (the FST shares that
//! level schedule). So domains here are powers of
//! two whose widths add up to a multiple of 6, padding the predicate
//! dimension, which is cheap since predicates are few.
//!
//! Subjects and objects share the dictionary's id space, so both get the
//! same width. Growth at least doubles the subject/object domain, keeping
//! rebuilds amortized O(1) per inserted triple.

/// Smallest subject/object width: room for a small dataset's ids without
/// an immediate rebuild.
pub const MIN_TERM_BITS: u32 = 8;
/// Smallest predicate width.
pub const MIN_PREDICATE_BITS: u32 = 1;

/// The bit widths of a partition's domains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Domains {
    pub term_bits: u32,
    pub predicate_bits: u32,
}

fn bits_for(max_value: u32) -> u32 {
    u32::BITS - max_value.leading_zeros()
}

impl Domains {
    /// Domains holding term ids up to `max_term` and predicate ids up to
    /// `max_predicate`.
    pub fn fitting(max_term: u32, max_predicate: u32) -> Self {
        Self::aligned(bits_for(max_term).max(MIN_TERM_BITS), bits_for(max_predicate).max(MIN_PREDICATE_BITS))
    }

    fn aligned(term_bits: u32, predicate_bits: u32) -> Self {
        let mut predicate_bits = predicate_bits;
        while !(2 * term_bits + predicate_bits).is_multiple_of(6) {
            predicate_bits += 1;
        }
        Self { term_bits, predicate_bits }
    }

    /// Domain sizes in s-graph's `[subject, predicate, object]` order.
    pub fn sizes(self) -> [u64; 3] {
        [1u64 << self.term_bits, 1u64 << self.predicate_bits, 1u64 << self.term_bits]
    }

    pub fn holds(self, s: u32, p: u32, o: u32) -> bool {
        bits_for(s.max(o)) <= self.term_bits && bits_for(p) <= self.predicate_bits
    }

    /// Domains holding term ids up to `max_term` and predicate ids up to
    /// `max_predicate`, never narrower than these. A subject/object domain
    /// that has to grow at least doubles; the predicate width is recomputed
    /// from `max_predicate` (callers pass the largest predicate id in use,
    /// so old alignment padding is not carried over).
    pub fn grown(self, max_term: u32, max_predicate: u32) -> Self {
        let need = bits_for(max_term);
        let term_bits = if need > self.term_bits { need.max(self.term_bits + 1) } else { self.term_bits };
        Self::aligned(term_bits, bits_for(max_predicate).max(MIN_PREDICATE_BITS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total_bits(d: Domains) -> u32 {
        2 * d.term_bits + d.predicate_bits
    }

    #[test]
    fn fitting_domains_are_aligned_to_whole_levels() {
        for (t, p) in [(0, 0), (1, 1), (255, 3), (256, 3), (1 << 20, 17), (u32::MAX, 1000)] {
            let d = Domains::fitting(t, p);
            assert_eq!(total_bits(d) % 6, 0, "{d:?}");
            assert!(d.holds(t, p, t), "{d:?} must hold ({t}, {p})");
            assert!(d.term_bits >= MIN_TERM_BITS);
        }
    }

    #[test]
    fn padding_goes_to_predicates() {
        // 8 term bits -> 16, so predicates pad to 2 (total 18).
        assert_eq!(Domains::fitting(200, 1), Domains { term_bits: 8, predicate_bits: 2 });
        // 9 term bits -> 18, predicates pad from 2 to 6 (total 24).
        assert_eq!(Domains::fitting(300, 3), Domains { term_bits: 9, predicate_bits: 6 });
    }

    #[test]
    fn sizes_are_subject_predicate_object() {
        let d = Domains { term_bits: 8, predicate_bits: 2 };
        assert_eq!(d.sizes(), [256, 4, 256]);
    }

    #[test]
    fn growth_at_least_doubles_and_stays_aligned() {
        let d = Domains::fitting(255, 1);
        let g = d.grown(256, 1);
        assert!(g.term_bits > d.term_bits);
        assert!(g.holds(256, 1, 0));
        assert_eq!(total_bits(g) % 6, 0);

        let g = d.grown(0, 1000);
        assert_eq!(g.term_bits, d.term_bits);
        assert!(g.holds(0, 1000, 0));
        assert_eq!(total_bits(g) % 6, 0);
    }

    #[test]
    fn growth_is_a_no_op_when_the_triple_fits() {
        let d = Domains::fitting(1000, 50);
        assert!(d.holds(1000, 50, 7));
        let g = d.grown(1000, 50);
        assert!(g.holds(1000, 50, 7));
        assert_eq!(g.term_bits, d.term_bits);
    }
}
