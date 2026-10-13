//! The store: a [`Dictionary`] and a [`QuadIndex`], composed.
//!
//! Term-level operations take canonical keys ([`crate::term`]) and resolve
//! them to ids; id-level access goes straight to the index. Bulk ingestion
//! lives in [`crate::load`], queries in [`crate::sparql`].

use crate::dict::Dictionary;
use crate::error::{Result, StoreError};
use crate::graph::{GraphName, IdPattern, QuadIndex, Quads};
use crate::term::{BlankScope, TermRef};
use s_dict::TermId;
use s_graph::storage::HeapStorage;
use s_graph::BlockStorage;

/// A graph of the dataset, by key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphRef<'a> {
    Default,
    Named(&'a str),
}

/// A quad selection by key; `None` matches anything. `graph: None` covers
/// the default graph and every named graph.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuadPattern<'a> {
    pub graph: Option<GraphRef<'a>>,
    pub subject: Option<&'a str>,
    pub predicate: Option<&'a str>,
    pub object: Option<&'a str>,
}

#[derive(Clone, Copy, Debug)]
pub struct StoreConfig {
    /// s-graph block capacity of every graph tree.
    pub block_capacity: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self { block_capacity: s_graph::DEFAULT_BLOCK_CAPACITY }
    }
}

pub struct Store<S: BlockStorage = HeapStorage> {
    dict: Dictionary,
    quads: QuadIndex<S>,
    next_blank_scope: u64,
}

impl Store<HeapStorage> {
    pub fn new() -> Self {
        Self::with_config(StoreConfig::default())
    }
}

impl Default for Store<HeapStorage> {
    fn default() -> Self {
        Self::new()
    }
}

/// Which positions a key may take.
#[derive(Clone, Copy)]
enum Position {
    Subject,
    Predicate,
    Object,
    Graph,
}

fn check_key(key: &str, position: Position) -> Result<()> {
    let ok = match (TermRef::parse(key), position) {
        (None, _) => false,
        (Some(TermRef::Iri(_)), _) => true,
        (Some(_), Position::Predicate) => false,
        (Some(TermRef::Literal { .. }), Position::Subject | Position::Graph) => false,
        (Some(TermRef::Triple(_)), Position::Graph) => false,
        _ => true,
    };
    if ok { Ok(()) } else { Err(StoreError::InvalidKey(key.to_owned())) }
}

impl<S: BlockStorage> Store<S> {
    pub fn with_config(config: StoreConfig) -> Self {
        Self { dict: Dictionary::new(), quads: QuadIndex::new(config.block_capacity), next_blank_scope: 0 }
    }

    /// Reassembles a store from persisted parts.
    pub fn from_parts(dict: Dictionary, quads: QuadIndex<S>, next_blank_scope: u64) -> Self {
        Self { dict, quads, next_blank_scope }
    }

    pub fn dictionary(&self) -> &Dictionary {
        &self.dict
    }

    pub fn index(&self) -> &QuadIndex<S> {
        &self.quads
    }

    /// Both halves, mutably, for ingestion paths that intern and insert in
    /// batches ([`crate::load`]).
    pub fn parts_mut(&mut self) -> (&mut Dictionary, &mut QuadIndex<S>) {
        (&mut self.dict, &mut self.quads)
    }

    /// A blank node scope no earlier load or update used.
    pub fn new_blank_scope(&mut self) -> BlankScope {
        self.next_blank_scope += 1;
        BlankScope(self.next_blank_scope)
    }

    pub fn next_blank_scope(&self) -> u64 {
        self.next_blank_scope
    }

    pub fn len(&self) -> u64 {
        self.quads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.quads.is_empty()
    }

    /// Interns `graph`'s name.
    pub fn graph_name(&mut self, graph: GraphRef<'_>) -> Result<GraphName> {
        match graph {
            GraphRef::Default => Ok(GraphName::Default),
            GraphRef::Named(key) => {
                check_key(key, Position::Graph)?;
                Ok(GraphName::Named(self.dict.intern(key)?))
            }
        }
    }

    /// Resolves `graph`'s name without interning; `None` if unknown.
    pub fn lookup_graph(&self, graph: GraphRef<'_>) -> Option<GraphName> {
        match graph {
            GraphRef::Default => Some(GraphName::Default),
            GraphRef::Named(key) => self.dict.lookup(key).map(GraphName::Named),
        }
    }

    /// Inserts a quad of canonical keys; `Ok(false)` if already present.
    pub fn insert(&mut self, graph: GraphRef<'_>, s: &str, p: &str, o: &str) -> Result<bool> {
        check_key(s, Position::Subject)?;
        check_key(p, Position::Predicate)?;
        check_key(o, Position::Object)?;
        let g = self.graph_name(graph)?;
        let (s, p, o) = (self.dict.intern(s)?, self.dict.intern(p)?, self.dict.intern(o)?);
        Ok(self.quads.insert(g, s, p, o))
    }

    /// Removes a quad; `false` if absent. Terms stay in the dictionary.
    pub fn remove(&mut self, graph: GraphRef<'_>, s: &str, p: &str, o: &str) -> bool {
        match self.lookup_quad(graph, s, p, o) {
            Some((g, s, p, o)) => self.quads.remove(g, s, p, o),
            None => false,
        }
    }

    pub fn contains(&self, graph: GraphRef<'_>, s: &str, p: &str, o: &str) -> bool {
        self.lookup_quad(graph, s, p, o).is_some_and(|(g, s, p, o)| self.quads.contains(g, s, p, o))
    }

    fn lookup_quad(&self, graph: GraphRef<'_>, s: &str, p: &str, o: &str) -> Option<(GraphName, TermId, TermId, TermId)> {
        Some((self.lookup_graph(graph)?, self.dict.lookup(s)?, self.dict.lookup(p)?, self.dict.lookup(o)?))
    }

    /// Streams matching id quads; a constant missing from the dictionary
    /// matches nothing. Resolve ids with [`Self::key`] or
    /// [`Dictionary::key_into`].
    pub fn find(&self, pattern: QuadPattern<'_>) -> Quads<'_, S> {
        let resolve = |key: Option<&str>| -> std::result::Result<Option<TermId>, ()> {
            match key {
                None => Ok(None),
                Some(k) => self.dict.lookup(k).map(Some).ok_or(()),
            }
        };
        let id_pattern = (|| {
            Some(IdPattern {
                graph: match pattern.graph {
                    None => None,
                    Some(g) => Some(self.lookup_graph(g)?),
                },
                subject: resolve(pattern.subject).ok()?,
                predicate: resolve(pattern.predicate).ok()?,
                object: resolve(pattern.object).ok()?,
            })
        })();
        match id_pattern {
            Some(p) => self.quads.quads(p),
            // An unknown constant: an unknown predicate selects nothing.
            None => self.quads.quads(IdPattern { predicate: Some(TermId(u32::MAX)), ..Default::default() }),
        }
    }

    pub fn key(&self, id: TermId) -> Option<String> {
        self.dict.key(id)
    }

    /// Ends a bulk load: compacts the dictionary and trims tree buffers.
    pub fn shrink_to_fit(&mut self) {
        self.dict.shrink_to_fit();
        self.quads.shrink_to_fit();
    }

    /// Logical bytes of the dictionary plus the graph trees.
    pub fn size_bytes(&self) -> usize {
        self.dict.size_bytes() + self.quads.mem_size_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: GraphRef<'static> = GraphRef::Named("<http://g>");

    fn keys(store: &Store, pattern: QuadPattern<'_>) -> Vec<[String; 4]> {
        let mut v: Vec<[String; 4]> = store
            .find(pattern)
            .map(|(g, s, p, o)| {
                let g = match g {
                    GraphName::Default => String::new(),
                    GraphName::Named(g) => store.key(g).unwrap(),
                };
                [g, store.key(s).unwrap(), store.key(p).unwrap(), store.key(o).unwrap()]
            })
            .collect();
        v.sort();
        v
    }

    #[test]
    fn insert_contains_remove_by_key() {
        let mut st = Store::new();
        assert!(st.insert(GraphRef::Default, "<a>", "<p>", "\"x\"").unwrap());
        assert!(!st.insert(GraphRef::Default, "<a>", "<p>", "\"x\"").unwrap());
        assert!(st.insert(G, "<a>", "<p>", "\"x\"").unwrap());
        assert_eq!(st.len(), 2);
        assert!(st.contains(GraphRef::Default, "<a>", "<p>", "\"x\""));
        assert!(st.contains(G, "<a>", "<p>", "\"x\""));
        assert!(!st.contains(GraphRef::Named("<http://other>"), "<a>", "<p>", "\"x\""));
        assert!(!st.contains(GraphRef::Default, "<a>", "<p>", "\"y\""));
        assert!(st.remove(G, "<a>", "<p>", "\"x\""));
        assert!(!st.remove(G, "<a>", "<p>", "\"x\""));
        assert_eq!(st.len(), 1);
    }

    #[test]
    fn rejects_keys_in_invalid_positions() {
        let mut st = Store::new();
        for (s, p, o) in [("\"lit\"", "<p>", "<o>"), ("<s>", "_:b", "<o>"), ("<s>", "\"p\"", "<o>"), ("s", "<p>", "<o>"), ("<s>", "<p>", "o\"")] {
            assert!(matches!(st.insert(GraphRef::Default, s, p, o), Err(StoreError::InvalidKey(_))), "{s} {p} {o}");
        }
        assert!(st.insert(GraphRef::Named("\"g\""), "<s>", "<p>", "<o>").is_err());
        assert!(st.insert(GraphRef::Default, "_:b1_x", "<p>", "<<( <s> <p> <o> )>>").is_ok());
        assert!(st.is_empty() || st.len() == 1);
    }

    #[test]
    fn find_by_key_pattern() {
        let mut st = Store::new();
        st.insert(GraphRef::Default, "<a>", "<p>", "<b>").unwrap();
        st.insert(G, "<a>", "<q>", "<c>").unwrap();
        st.insert(G, "<d>", "<p>", "<b>").unwrap();
        assert_eq!(keys(&st, QuadPattern::default()).len(), 3);
        assert_eq!(
            keys(&st, QuadPattern { subject: Some("<a>"), ..Default::default() }),
            vec![
                [String::new(), "<a>".into(), "<p>".into(), "<b>".into()],
                ["<http://g>".into(), "<a>".into(), "<q>".into(), "<c>".into()],
            ]
        );
        assert_eq!(keys(&st, QuadPattern { graph: Some(G), predicate: Some("<p>"), ..Default::default() }).len(), 1);
        assert_eq!(keys(&st, QuadPattern { object: Some("<nope>"), ..Default::default() }), Vec::<[String; 4]>::new());
        assert_eq!(keys(&st, QuadPattern { graph: Some(GraphRef::Named("<nope>")), ..Default::default() }), Vec::<[String; 4]>::new());
    }

    #[test]
    fn blank_scopes_are_fresh() {
        let mut st = Store::new();
        let a = st.new_blank_scope();
        let b = st.new_blank_scope();
        assert_ne!(a, b);
    }
}
