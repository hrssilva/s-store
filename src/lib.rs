//! `s-store`: an RDF quad store built on three sibling libraries:
//!
//! - [`s_dict`] interns every RDF term as a `u32` [`TermId`]; the dictionary
//!   key is the term's canonical N-Triples serialization ([`term`]).
//! - [`s_graph`] indexes `(subject, predicate, object)` id triples. The store
//!   keeps one FST per graph (the default graph and each named graph), so a
//!   quad costs exactly what its triple costs in one FST ([`graph`]), and
//!   runs BGPs as s-graph automata over it ([`sparql`]).
//! - [`s_parser`] feeds data (N-Triples, Turtle) and SPARQL queries; terms are
//!   encoded straight from the parser's borrowed spans ([`load`], [`sparql`]).
//!
//! Property graphs are layered on top experimentally ([`property`]): node and
//! edge properties are triples in a dedicated partition, and an edge's
//! identity is an RDF 1.2 triple term interned in the same dictionary.
//!
//! See `DESIGN.md` for the decisions behind each layer.

pub mod dict;
pub mod error;
pub mod graph;
pub mod load;
pub mod property;
pub mod sparql;
pub mod store;
pub mod term;

pub use error::{Result, StoreError};
pub use graph::GraphName;
pub use s_dict::TermId;
pub use store::{GraphRef, QuadPattern, Store, StoreConfig};
pub use term::TermRef;

/// Compiles and runs the README example as a doctest.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
