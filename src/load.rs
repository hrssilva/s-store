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
//! 4. A load into a store whose dictionary was empty (so it issued every id
//!    in the store) renumbers the dictionary to term order before the bulk
//!    build, remapping the collected triples and the graph name: every id
//!    becomes its term's rank, which compresses the graph better than
//!    first-seen order and makes the ids independent of batching.
//!
//! Blank node labels are scoped to the load ([`BlankScope`]).
//!
//! [`load_parallel`] splits N-Triples input into byte ranges parsed by
//! worker threads, which also prepare (sort and group) their batches; the
//! calling thread interns them, taking one batch from each worker in turn so
//! the ids are the same on every run.

use crate::error::{Result, StoreError};
use crate::graph::GraphName;
use crate::store::{GraphRef, Store};
use crate::term::{BlankScope, KeyBuf, nt_fast_key};
use s_dict::{PreparedBatch, TermBatch, TermId};
use s_graph::{BlockStorage, Triple};
use s_parser::{NtTokenizer, QuadReader, SeekableStream, SliceStream, Stream, TurtleReader, TurtleTokenizer};
use std::sync::mpsc;

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

/// Loads every record of `stream`. Into a store with an empty dictionary,
/// the ids end up in term order (see the module docs).
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

/// Loads N-Triples with `workers` parser threads, each reading one byte
/// range (line-aligned) of its own stream from `open` (for a
/// `s_parser::FileStream`, `|| file.clone()`); the calling
/// thread interns their batches and builds the graph. The store ends up
/// with the same quads as with [`load`].
///
/// Batches are interned in arrival order, not file order; like [`load`],
/// a load into a store whose dictionary was empty ends with term-order
/// ids, here independent of `workers`. Otherwise ids are the same on every
/// run with the same `workers` and `batch_triples`. `workers <= 1` is
/// [`load`].
pub fn load_parallel<S: BlockStorage, St: SeekableStream + Send>(store: &mut Store<S>, open: impl Fn() -> St + Sync, options: &LoadOptions<'_>, workers: usize) -> Result<LoadStats> {
    if workers <= 1 {
        return load(store, open(), Format::NTriples, options);
    }
    let mut batcher = Batcher::new(store, options)?;
    let (scope, batch_triples, len) = (batcher.scope, batcher.batch_triples, open().len());
    std::thread::scope(|threads| {
        let mut ready = Vec::with_capacity(workers);
        let mut recycle = Vec::with_capacity(workers);
        let mut handles = Vec::with_capacity(workers);
        for worker in 0..workers {
            let boundary = |i: usize| (len as u128 * i as u128 / workers as u128) as u64;
            let range = boundary(worker)..boundary(worker + 1);
            let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<(PreparedBatch, usize)>>(1);
            let (recycle_tx, recycle_rx) = mpsc::sync_channel::<TermBatch>(1);
            ready.push(Some(ready_rx));
            recycle.push(recycle_tx);
            let stream = open();
            handles.push(threads.spawn(move || {
                let parse = || -> Result<()> {
                    let mut reader = QuadReader::for_range(stream, NtTokenizer, range)?;
                    let mut key = KeyBuf::new();
                    let mut batch = TermBatch::new();
                    loop {
                        let mut triples = 0;
                        while triples < batch_triples {
                            let Some(quad) = reader.next()? else { break };
                            if quad.graph().is_some() {
                                return Err(StoreError::Query("N-Quads graph terms are not supported by the loader yet".into()));
                            }
                            for term in [quad.subject(), quad.predicate(), quad.object()] {
                                match nt_fast_key(term) {
                                    Some(k) => batch.push(k),
                                    None => batch.push(key.resolved(term.resolve(), scope, None)?),
                                }
                            }
                            triples += 1;
                        }
                        if triples == 0 {
                            return Ok(());
                        }
                        // The writer stops receiving only on an error of its own.
                        if ready_tx.send(Ok((batch.prepare(), triples))).is_err() {
                            return Ok(());
                        }
                        batch = match recycle_rx.recv() {
                            Ok(batch) => batch,
                            Err(_) => return Ok(()),
                        };
                    }
                };
                if let Err(e) = parse() {
                    let _ = ready_tx.send(Err(e));
                }
            }));
        }
        let mut result = Ok(());
        'writer: while ready.iter().any(Option::is_some) {
            for worker in 0..workers {
                let Some(rx) = &ready[worker] else { continue };
                match rx.recv() {
                    Ok(Ok((prepared, triples))) => {
                        if let Err(e) = batcher.intern(&prepared, triples) {
                            result = Err(e);
                            break 'writer;
                        }
                        let mut batch = prepared.into_batch();
                        batch.clear();
                        // The worker may have finished its range already.
                        let _ = recycle[worker].send(batch);
                    }
                    Ok(Err(e)) => {
                        result = Err(e);
                        break 'writer;
                    }
                    Err(_) => ready[worker] = None,
                }
            }
        }
        // Wake workers blocked on either channel, so they can be joined.
        drop(ready);
        drop(recycle);
        for handle in handles {
            if handle.join().is_err() && result.is_ok() {
                result = Err(StoreError::Query("a parser worker panicked".into()));
            }
        }
        result
    })?;
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
    /// In bulk mode, renumber the dictionary to term order before the
    /// build: set only when this load issued every id in the store.
    term_order: bool,
    stats: LoadStats,
}

impl<'s, S: BlockStorage> Batcher<'s, S> {
    fn new(store: &'s mut Store<S>, options: &LoadOptions<'_>) -> Result<Self> {
        // Before the graph name is interned: an empty dictionary means this
        // load will issue every id in the store.
        let fresh = store.dictionary().is_empty();
        let graph = store.graph_name(options.graph)?;
        let bulk = store.index().partition(graph).is_none_or(|p| p.is_empty());
        let scope = store.new_blank_scope();
        Ok(Self { store, graph, scope, batch: TermBatch::new(), batch_triples: options.batch_triples.max(1), pending: 0, triples: Vec::new(), bulk, term_order: fresh && bulk, stats: LoadStats::default() })
    }

    fn push_term(&mut self, key: &str) {
        self.batch.push(key);
    }

    fn end_triple(&mut self) -> Result<()> {
        self.pending += 1;
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
        let pending = std::mem::take(&mut self.pending);
        let interned = self.intern(&prepared, pending);
        self.batch = prepared.into_batch();
        self.batch.clear();
        interned
    }

    /// Interns a prepared batch of `triples` triples (three terms each, in
    /// order) and adds them to the graph (or, in bulk mode, to the pending
    /// triples).
    fn intern(&mut self, prepared: &PreparedBatch, triples: usize) -> Result<()> {
        let (dict, index) = self.store.parts_mut();
        let ids = dict.intern_prepared(prepared)?;
        self.stats.triples += triples as u64;
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
            let mut triples = std::mem::take(&mut self.triples);
            if self.term_order {
                let map = self.store.parts_mut().0.renumber_term_order();
                for t in &mut triples {
                    *t = (map[t.0 as usize], map[t.1 as usize], map[t.2 as usize]);
                }
                if let GraphName::Named(g) = self.graph {
                    self.graph = GraphName::Named(TermId(map[g.0 as usize]));
                }
            }
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

    fn sample_nt() -> String {
        let mut nt = String::new();
        for i in 0..500 {
            nt.push_str(&format!("<http://s{}> <http://p{}> \"v{i}\"@EN .\n_:b{} <http://p0> <http://s{}> .\n", i % 37, i % 3, i % 11, i % 13));
        }
        nt
    }

    fn id_quads(st: &Store) -> Vec<(GraphName, TermId, TermId, TermId)> {
        st.find(QuadPattern::default()).collect()
    }

    #[test]
    fn fresh_loads_get_term_order_ids_for_any_workers() {
        let nt = sample_nt();
        let mut seq = Store::new();
        load_slice(&mut seq, nt.as_bytes(), Format::NTriples, &LoadOptions::default()).unwrap();
        let expected = all_keys(&seq, None);
        let options = LoadOptions { batch_triples: 7, ..Default::default() };
        let mut first: Option<Vec<_>> = None;
        for workers in [1, 2, 3, 8] {
            let mut st = Store::new();
            let stats = load_parallel(&mut st, || SliceStream::new(nt.as_bytes(), 256).unwrap(), &options, workers).unwrap();
            assert_eq!(stats.triples, 1000, "{workers} workers");
            assert_eq!(all_keys(&st, None), expected, "{workers} workers");
            // Every id is its term's rank: ids 0..len in increasing key order.
            let keys: Vec<String> = (0..st.dictionary().len() as u32).map(|id| st.key(TermId(id)).expect("dense ids")).collect();
            assert!(keys.windows(2).all(|w| w[0] < w[1]), "{workers} workers: ids not in term order");
            assert_eq!(st.dictionary().id_bound() as usize, keys.len());
            // So the ids depend neither on the worker count nor on batching.
            let quads = id_quads(&st);
            assert_eq!(*first.get_or_insert_with(|| quads.clone()), quads, "{workers} workers");
        }
        assert_eq!(id_quads(&seq), first.unwrap(), "sequential load with default batches");
    }

    #[test]
    fn parallel_load_renumbers_into_a_named_graph() {
        let nt = sample_nt();
        let options = LoadOptions { graph: GraphRef::Named("<http://g>"), batch_triples: 7, ..Default::default() };
        let mut st = Store::new();
        load_parallel(&mut st, || SliceStream::new(nt.as_bytes(), 256).unwrap(), &options, 4).unwrap();
        let mut seq = Store::new();
        load_slice(&mut seq, nt.as_bytes(), Format::NTriples, &options).unwrap();
        let named = Some(GraphRef::Named("<http://g>"));
        assert_eq!(all_keys(&st, named), all_keys(&seq, named));
        assert!(all_keys(&st, Some(GraphRef::Default)).is_empty());
        assert!(st.contains(GraphRef::Named("<http://g>"), "<http://s1>", "<http://p1>", "\"v1\"@en"));
    }

    #[test]
    fn parallel_load_into_a_non_empty_store_keeps_existing_ids() {
        let mut st = Store::new();
        st.insert(GraphRef::Default, "<http://z>", "<http://p0>", "<http://a>").unwrap();
        let before = id_quads(&st);
        let nt = sample_nt();
        let options = LoadOptions { graph: GraphRef::Named("<http://g>"), batch_triples: 7, ..Default::default() };
        let stats = load_parallel(&mut st, || SliceStream::new(nt.as_bytes(), 256).unwrap(), &options, 3).unwrap();
        assert_eq!(stats.triples, 1000);
        assert_eq!(id_quads(&st).into_iter().filter(|q| q.0 == GraphName::Default).collect::<Vec<_>>(), before);
        assert!(st.contains(GraphRef::Default, "<http://z>", "<http://p0>", "<http://a>"));
        assert!(st.contains(GraphRef::Named("<http://g>"), "<http://s1>", "<http://p1>", "\"v1\"@en"));
    }

    #[test]
    fn parallel_load_of_nothing() {
        let mut st = Store::new();
        let stats = load_parallel(&mut st, || SliceStream::new(b"", 64).unwrap(), &LoadOptions::default(), 4).unwrap();
        assert_eq!(stats, LoadStats::default());
        assert!(st.is_empty());
    }

    #[test]
    fn parallel_load_errors_surface() {
        let nt = "<http://a> <http://p> <http://b> .\n".repeat(100) + "<a> <b> .\n" + &"<http://a> <http://p> <http://c> .\n".repeat(100);
        let mut st = Store::new();
        let r = load_parallel(&mut st, || SliceStream::new(nt.as_bytes(), 256).unwrap(), &LoadOptions { batch_triples: 5, ..Default::default() }, 4);
        assert!(matches!(r, Err(StoreError::Parse(_))), "{r:?}");
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
