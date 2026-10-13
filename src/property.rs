//! Experimental property-graph layer.
//!
//! A property graph's nodes are RDF terms and its edges are triples of the
//! dataset. Properties attach key/value pairs to either:
//!
//! - **Node properties** are triples `(node, key, value)`.
//! - **Edge properties** are triples `(edge, key, value)` where `edge` is the
//!   edge's RDF 1.2 triple term `<<( s p o )>>`, interned in the same
//!   dictionary as every other term.
//!
//! All property triples live in one reserved named graph, [`PROPERTIES_GRAPH`],
//! so they cost exactly what any other triple costs and need no structure of
//! their own. SPARQL reaches them explicitly with `GRAPH <urn:x-s-store:properties>`;
//! the query engine leaves the graph out of `GRAPH ?g` enumeration and the
//! union default graph, so property data never leaks into RDF answers.
//!
//! Property graphs have one value per key: [`PropertiesMut::set`] replaces.
//! Keys are IRIs (`<...>` keys); values are any term key.

use crate::error::{Result, StoreError};
use crate::graph::{GraphName, IdPattern};
use crate::store::{GraphRef, Store};
use crate::term::{KeyBuf, TermRef};
use s_dict::TermId;
use s_graph::BlockStorage;

/// The reserved graph holding property triples.
pub const PROPERTIES_GRAPH: &str = "<urn:x-s-store:properties>";

/// The owner of a property: a node (term key) or an edge (triple of keys).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner<'a> {
    Node(&'a str),
    Edge(&'a str, &'a str, &'a str),
}

impl<'a> Owner<'a> {
    fn key<'b>(self, buf: &'b mut KeyBuf) -> &'b str
    where
        'a: 'b,
    {
        match self {
            Owner::Node(key) => key,
            Owner::Edge(s, p, o) => buf.triple(s, p, o),
        }
    }
}

/// Read access to properties.
pub struct Properties<'s, S: BlockStorage> {
    store: &'s Store<S>,
}

/// Write access to properties.
pub struct PropertiesMut<'s, S: BlockStorage> {
    store: &'s mut Store<S>,
}

impl<S: BlockStorage> Store<S> {
    pub fn properties(&self) -> Properties<'_, S> {
        Properties { store: self }
    }

    pub fn properties_mut(&mut self) -> PropertiesMut<'_, S> {
        PropertiesMut { store: self }
    }

    /// The reserved properties graph's id, if any property was ever set.
    pub fn properties_graph(&self) -> Option<GraphName> {
        self.lookup_graph(GraphRef::Named(PROPERTIES_GRAPH))
    }
}

impl<S: BlockStorage> Properties<'_, S> {
    fn owner_id(&self, owner: Owner<'_>) -> Option<TermId> {
        let mut buf = KeyBuf::new();
        self.store.dictionary().lookup(owner.key(&mut buf))
    }

    /// The value of `key` on `owner`.
    pub fn get(&self, owner: Owner<'_>, key: &str) -> Option<TermId> {
        let graph = self.store.properties_graph()?;
        let pattern = IdPattern { graph: Some(graph), subject: Some(self.owner_id(owner)?), predicate: Some(self.store.dictionary().lookup(key)?), object: None };
        self.store.index().quads(pattern).next().map(|q| q.3)
    }

    /// Every `(key, value)` of `owner`, in no particular order.
    pub fn all(&self, owner: Owner<'_>) -> impl Iterator<Item = (TermId, TermId)> + '_ {
        let pattern = self.store.properties_graph().zip(self.owner_id(owner)).map(|(graph, owner)| IdPattern { graph: Some(graph), subject: Some(owner), ..Default::default() });
        pattern.into_iter().flat_map(|p| self.store.index().quads(p).map(|q| (q.2, q.3)))
    }

    /// The edge `(s, p, o)` an edge-owner id stands for, as keys.
    pub fn edge_of(&self, owner: TermId) -> Option<[String; 3]> {
        let key = self.store.key(owner)?;
        let TermRef::Triple(inner) = TermRef::parse(&key)? else { return None };
        let [s, p, o] = TermRef::triple_parts(inner)?;
        Some([s.to_owned(), p.to_owned(), o.to_owned()])
    }
}

impl<S: BlockStorage> PropertiesMut<'_, S> {
    /// Sets `key` on `owner` to `value`, replacing any previous value.
    /// Returns whether the property changed.
    pub fn set(&mut self, owner: Owner<'_>, key: &str, value: &str) -> Result<bool> {
        if !matches!(TermRef::parse(key), Some(TermRef::Iri(_))) {
            return Err(StoreError::InvalidKey(key.to_owned()));
        }
        let mut buf = KeyBuf::new();
        let owner_key = owner.key(&mut buf);
        let previous = self.store.properties().get(owner, key);
        let graph = GraphRef::Named(PROPERTIES_GRAPH);
        if let Some(prev) = previous {
            if self.store.dictionary().lookup(value) == Some(prev) {
                return Ok(false);
            }
            let prev_key = self.store.key(prev).expect("stored ids resolve");
            self.store.remove(graph, owner_key, key, &prev_key);
        }
        self.store.insert(graph, owner_key, key, value)
    }

    /// Removes `key` from `owner`; returns whether it was set.
    pub fn remove(&mut self, owner: Owner<'_>, key: &str) -> bool {
        let Some(prev) = self.store.properties().get(owner, key) else { return false };
        let mut buf = KeyBuf::new();
        let owner_key = owner.key(&mut buf).to_owned();
        let prev_key = self.store.key(prev).expect("stored ids resolve");
        self.store.remove(GraphRef::Named(PROPERTIES_GRAPH), &owner_key, key, &prev_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::QuadPattern;

    const NAME: &str = "<http://schema.org/name>";
    const SINCE: &str = "<http://ex/since>";

    fn value(store: &Store, id: Option<TermId>) -> Option<String> {
        id.and_then(|id| store.key(id))
    }

    #[test]
    fn node_properties_have_one_value_per_key() {
        let mut st = Store::new();
        let alice = Owner::Node("<http://ex/alice>");
        assert!(st.properties_mut().set(alice, NAME, "\"Alice\"").unwrap());
        assert!(!st.properties_mut().set(alice, NAME, "\"Alice\"").unwrap());
        assert!(st.properties_mut().set(alice, NAME, "\"Alicia\"").unwrap());
        assert_eq!(value(&st, st.properties().get(alice, NAME)).as_deref(), Some("\"Alicia\""));
        assert_eq!(st.properties().all(alice).count(), 1);
        assert!(st.properties_mut().remove(alice, NAME));
        assert!(!st.properties_mut().remove(alice, NAME));
        assert_eq!(st.properties().get(alice, NAME), None);
    }

    #[test]
    fn edge_properties_attach_to_triple_terms() {
        let mut st = Store::new();
        st.insert(GraphRef::Default, "<http://ex/alice>", "<http://ex/knows>", "<http://ex/bob>").unwrap();
        let edge = Owner::Edge("<http://ex/alice>", "<http://ex/knows>", "<http://ex/bob>");
        st.properties_mut().set(edge, SINCE, "\"2020\"^^<http://www.w3.org/2001/XMLSchema#gYear>").unwrap();
        st.properties_mut().set(edge, NAME, "\"friendship\"").unwrap();
        let mut props: Vec<String> = st.properties().all(edge).map(|(k, _)| st.key(k).unwrap()).collect();
        props.sort();
        assert_eq!(props, vec![SINCE.to_string(), NAME.to_string()]);

        let owner = st.dictionary().lookup("<<( <http://ex/alice> <http://ex/knows> <http://ex/bob> )>>").unwrap();
        assert_eq!(st.properties().edge_of(owner), Some(["<http://ex/alice>".into(), "<http://ex/knows>".into(), "<http://ex/bob>".into()]));
    }

    #[test]
    fn properties_stay_out_of_the_default_graph() {
        let mut st = Store::new();
        st.properties_mut().set(Owner::Node("<http://ex/a>"), NAME, "\"A\"").unwrap();
        assert_eq!(st.find(QuadPattern { graph: Some(GraphRef::Default), ..Default::default() }).count(), 0);
        assert_eq!(st.find(QuadPattern { graph: Some(GraphRef::Named(PROPERTIES_GRAPH)), ..Default::default() }).count(), 1);
    }

    #[test]
    fn keys_must_be_iris() {
        let mut st = Store::new();
        assert!(st.properties_mut().set(Owner::Node("<a>"), "\"name\"", "\"x\"").is_err());
    }

    #[test]
    fn unknown_owners_have_no_properties() {
        let st = Store::new();
        assert_eq!(st.properties().get(Owner::Node("<a>"), NAME), None);
        assert_eq!(st.properties().all(Owner::Edge("<a>", "<b>", "<c>")).count(), 0);
    }
}
