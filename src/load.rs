//! Bulk ingestion: parser records to dictionary batches to graph trees.
//!
//! The pipeline mirrors s-bench's `nt_dict_file_bench ... graph`:
//!
//! 1. Each parsed term becomes its canonical key: N-Triples terms that are
//!    already canonical are borrowed from the parser's window
//!    ([`nt_fast_key`]); the rest are encoded into one reused [`KeyBuf`].
//! 2. Keys are pushed into an s-dict [`TermBatch`] (the one copy of term
//!    text, needed because the parser's window moves on), which is prepared
//!    (sorted and grouped) and interned in one `insert_prepared` call per
//!    batch. The batch's buffers are reused across batches.
//! 3. The returned ids become `(s, p, o)` triples. Loading into an empty
//!    graph collects them (12 bytes per triple) and bulk-builds the graph's
//!    tree once at the end; loading into a non-empty graph inserts each
//!    batch with s-graph's Morton-sorted `insert_batch`.
//!
//! Blank node labels are scoped to the load ([`BlankScope`]).

use crate::error::{Result, StoreError};
use crate::graph::GraphName;
use crate::store::{GraphRef, Store};
use crate::term::{BlankScope, KeyBuf, nt_fast_key};
use s_dict::{TermBatch, TermId};
use s_graph::{BlockStorage, Triple};
use s_parser::{NtTokenizer, QuadReader, SliceStream, Stream, TurtleReader, TurtleTokenizer};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    NTriples,
    Turtle,
}

impl Format {
    /// From a file name's extension (`.nt`, `.ttl`).
    pub fn from_path(path: &std::path::Path) -> Option<Self> {
        match path.extension()?.to_str()? {
            "nt" => Some(Format::NTriples),
            "ttl" => Some(Format::Turtle),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LoadOptions<'a> {
    /// The graph receiving every triple.
    pub graph: GraphRef<'a>,
    /// Base IRI for relative IRIs (Turtle). s-parser does not report
    /// in-document `@base` directives, so only this base applies.
    pub base: Option<&'a str>,
    /// Triples per dictionary batch.
    pub batch_triples: usize,
    /// Parser window size: also the largest record (statement) accepted.
    pub window_bytes: usize,
}

impl Default for LoadOptions<'_> {
    fn default() -> Self {
        Self { graph: GraphRef::Default, base: None, batch_triples: 65_536, window_bytes: 4 << 20 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadStats {
    /// Triples parsed.
    pub triples: u64,
    /// Quads that were new to the store.
    pub inserted: u64,
}

/// Loads `bytes` already in memory.
pub fn load_slice<S: BlockStorage>(store: &mut Store<S>, bytes: &[u8], format: Format, options: &LoadOptions<'_>) -> Result<LoadStats> {
    let window = options.window_bytes.min(bytes.len().max(64));
    let stream = SliceStream::new(bytes, window)?;
    load(store, stream, format, options)
}

/// Loads a file, reading it into memory first. For files too large for
/// that, open an `s_parser::FileStream` (memory-mapped windows) or a
/// `GzStream` and call [`load`].
pub fn load_path<S: BlockStorage>(store: &mut Store<S>, path: &std::path::Path, format: Format, options: &LoadOptions<'_>) -> Result<LoadStats> {
    let bytes = std::fs::read(path)?;
    load_slice(store, &bytes, format, options)
}

/// Loads every record of `stream`.
pub fn load<S: BlockStorage, St: Stream>(store: &mut Store<S>, stream: St, format: Format, options: &LoadOptions<'_>) -> Result<LoadStats> {
    let mut batcher = Batcher::new(store, options)?;
    let mut key = KeyBuf::new();
    match format {
        Format::NTriples => {
            let mut reader = QuadReader::new(stream, NtTokenizer);
            while let Some(quad) = reader.next()? {
                if quad.graph().is_some() {
                    return Err(StoreError::Query("N-Quads graph terms are not supported by the loader yet".into()));
                }
                for term in [quad.subject(), quad.predicate(), quad.object()] {
                    match nt_fast_key(term) {
                        Some(k) => batcher.push_term(k),
                        None => batcher.push_term(key.resolved(term.resolve(), batcher.scope, None)?),
                    }
                }
                batcher.end_triple()?;
            }
        }
        Format::Turtle => {
            let mut reader = TurtleReader::new(stream, TurtleTokenizer::default());
            while let Some(statement) = reader.next()? {
                let record = statement.bytes();
                for t in statement.triples() {
                    for term in [&t.subject, &t.predicate, &t.object] {
                        batcher.push_term(key.resolved(term.resolve(record), batcher.scope, options.base)?);
                    }
                    batcher.end_triple()?;
                }
            }
        }
    }
    batcher.finish()
}

/// Accumulates keys into dictionary batches and id triples into the graph.
struct Batcher<'s, S: BlockStorage> {
    store: &'s mut Store<S>,
    graph: GraphName,
    scope: BlankScope,
    batch: TermBatch,
    batch_triples: usize,
    pending: usize,
    /// Id triples not yet in the graph (all of them, in bulk mode).
    triples: Vec<Triple>,
    /// The target graph was empty: build its tree once at the end.
    bulk: bool,
    stats: LoadStats,
}

impl<'s, S: BlockStorage> Batcher<'s, S> {
    fn new(store: &'s mut Store<S>, options: &LoadOptions<'_>) -> Result<Self> {
        let graph = store.graph_name(options.graph)?;
        let bulk = store.index().partition(graph).is_none_or(|p| p.is_empty());
        let scope = store.new_blank_scope();
        Ok(Self { store, graph, scope, batch: TermBatch::new(), batch_triples: options.batch_triples.max(1), pending: 0, triples: Vec::new(), bulk, stats: LoadStats::default() })
    }

    fn push_term(&mut self, key: &str) {
        self.batch.push(key);
    }

    fn end_triple(&mut self) -> Result<()> {
        self.pending += 1;
        self.stats.triples += 1;
        if self.pending >= self.batch_triples {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let prepared = std::mem::take(&mut self.batch).prepare();
        let (dict, index) = self.store.parts_mut();
        let ids = dict.intern_prepared(&prepared)?;
        self.batch = prepared.into_batch();
        self.batch.clear();
        self.pending = 0;
        self.triples.extend(ids.as_chunks::<3>().0.iter().map(|[s, p, o]: &[TermId; 3]| (s.0, p.0, o.0)));
        if !self.bulk {
            self.stats.inserted += index.insert_batch(self.graph, &mut self.triples);
            self.triples.clear();
        }
        Ok(())
    }

    fn finish(mut self) -> Result<LoadStats> {
        self.flush()?;
        if self.bulk {
            let triples = std::mem::take(&mut self.triples);
            self.stats.inserted += self.store.parts_mut().1.extend(self.graph, triples);
        }
        Ok(self.stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::QuadPattern;

    fn all_keys(store: &Store, graph: Option<GraphRef<'_>>) -> Vec<String> {
        let mut v: Vec<String> = store
            .find(QuadPattern { graph, ..Default::default() })
            .map(|(_, s, p, o)| format!("{} {} {}", store.key(s).unwrap(), store.key(p).unwrap(), store.key(o).unwrap()))
            .collect();
        v.sort();
        v
    }

    const NT: &str = r#"<http://a> <http://p> "x" .
<http://a> <http://p> "y\ty"@EN .
_:b1 <http://p> <http://a> .
<http://a> <http://p> "x" .
"#;

    #[test]
    fn loads_ntriples_with_canonical_keys_and_scoped_blanks() {
        let mut st = Store::new();
        let stats = load_slice(&mut st, NT.as_bytes(), Format::NTriples, &LoadOptions::default()).unwrap();
        assert_eq!(stats, LoadStats { triples: 4, inserted: 3 });
        let keys = all_keys(&st, None);
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&"<http://a> <http://p> \"y\\ty\"@en".to_string()), "{keys:?}");
        assert!(keys.iter().any(|k| k.starts_with("_:b1_b1 <http://p>")), "{keys:?}");
    }

    #[test]
    fn a_second_load_scopes_blank_nodes_apart_and_inserts_incrementally() {
        let mut st = Store::new();
        load_slice(&mut st, NT.as_bytes(), Format::NTriples, &LoadOptions::default()).unwrap();
        let stats = load_slice(&mut st, NT.as_bytes(), Format::NTriples, &LoadOptions { batch_triples: 2, ..Default::default() }).unwrap();
        assert_eq!(stats, LoadStats { triples: 4, inserted: 1 }, "only the re-scoped blank node triple is new");
        assert_eq!(st.len(), 4);
    }

    #[test]
    fn loads_turtle_into_a_named_graph_with_base() {
        let ttl = r#"@prefix ex: <http://ex/> .
ex:s ex:p 42, "a"^^<http://www.w3.org/2001/XMLSchema#string>, [ ex:q <rel> ] .
"#;
        let mut st = Store::new();
        let options = LoadOptions { graph: GraphRef::Named("<http://g>"), base: Some("http://base/dir/"), ..Default::default() };
        let stats = load_slice(&mut st, ttl.as_bytes(), Format::Turtle, &options).unwrap();
        assert_eq!(stats.inserted, 4);
        let keys = all_keys(&st, Some(GraphRef::Named("<http://g>")));
        assert!(keys.contains(&"<http://ex/s> <http://ex/p> \"42\"^^<http://www.w3.org/2001/XMLSchema#integer>".to_string()), "{keys:?}");
        assert!(keys.contains(&"<http://ex/s> <http://ex/p> \"a\"".to_string()), "{keys:?}");
        assert!(keys.iter().any(|k| k.ends_with("<http://ex/q> <http://base/dir/rel>")), "{keys:?}");
        assert!(all_keys(&st, Some(GraphRef::Default)).is_empty());
    }

    #[test]
    fn parse_errors_surface() {
        let mut st = Store::new();
        assert!(matches!(load_slice(&mut st, b"<a> <b> .\n", Format::NTriples, &LoadOptions::default()), Err(StoreError::Parse(_))));
    }

    #[test]
    fn format_from_extension() {
        assert_eq!(Format::from_path("x/y.nt".as_ref()), Some(Format::NTriples));
        assert_eq!(Format::from_path("y.ttl".as_ref()), Some(Format::Turtle));
        assert_eq!(Format::from_path("y.rdf".as_ref()), None);
    }
}
