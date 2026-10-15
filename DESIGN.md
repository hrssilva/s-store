# s-store design

An RDF quad store (SPARQL 1.1, experimental property graphs) composed from
`s-dict`, `s-graph` and `s-parser`. The store adds as little as possible on
top of them: no per-quad structure of its own, no secondary indexes, and no
intermediate solution sets where the algebra doesn't demand one.

## Layers

| Module | Responsibility | Depends on |
|---|---|---|
| `term` | canonical term keys; relative IRI resolution | s-parser term types |
| `dict` | key ↔ `u32` id (`Dictionary`, `Terms` trait) | s-dict |
| `graph` | id-level quads: one `Partition` (s-graph tree) per graph | s-graph |
| `store` | `Store` = `Dictionary` + `QuadIndex`, key-level API | dict, graph |
| `load` | batched ingestion (N-Triples, Turtle) | s-parser, s-dict batches |
| `property` | experimental property graph over a reserved graph | store |
| `sparql` | compile + evaluate SPARQL 1.1 queries | all of the above |

The query engine depends on narrow interfaces (`Terms`, partitions'
`scan`/tree, `PatternBackend`), so a Cypher front end can reuse `exec`'s
operators and `patterns`' s-graph delegation with its own compiler.

## Terms: the dictionary key is canonical N-Triples

`<iri>`, `_:label`, `"lex"`, `"lex"@lang`, `"lex"^^<dt>`, `<<( s p o )>>`.
RDF 1.2 canonical escaping, `xsd:string` folded into simple literals,
language tags lowercased, IRIs with escapes decoded and resolved against the
base. Consequences:

- **Id equality is term equality**, so joins, DISTINCT and GROUP BY compare
  `u32`s.
- N-Triples terms already in canonical form (the common case) are interned
  straight from the parser's window (`nt_fast_key`): no encoding, no copy
  besides s-dict's batch buffer.
- Keys decode by slicing (`TermRef::parse`), and serialize as N-Triples as-is.
- Blank nodes are scoped per load: `_:b{scope}_{label}` (written labels),
  `_:b{scope}-{n}` (parser-generated). Injective, and still valid N-Triples.

## Graph storage: one s-graph tree per graph

A quad `(g, s, p, o)` is the triple `(s, p', o)` in graph `g`'s tree.

- **S/O coordinates are dictionary ids** — no remap tables (s-bench's
  `DictSlack` layout). **Predicates** get a dense id space (`PredicateTable`:
  12 bytes per distinct predicate), otherwise the predicate dimension would
  be as wide as the term space.
- **Domains** are powers of two with total bit width a multiple of 6 (whole
  64-way trie levels; padding goes to the predicate dimension). s-bench
  measured up to 3.6× tree size for misaligned widths.
- **Growth**: s-graph can't widen a built tree, so an out-of-domain insert
  rebuilds by streaming the old tree's own `scan()` into
  `build_dynamic_bounded` (bounded memory, spills to disk). Domains at least
  double → amortized O(1) per insert.
- **Bulk load into an empty graph** collects id triples (12 B each) and
  bulk-builds once; loads into non-empty graphs use Morton-sorted
  `insert_batch` per dictionary batch.
- **Term-order ids after a fresh load**: when the dictionary was empty
  before a load (so nothing else holds an id: graph names, predicate
  table, other partitions), the loader renumbers it to term order
  (`Dictionary::renumber_term_order`, s-dict's `renumber`) and remaps the
  collected triples and the target graph name before the bulk build. Ids
  become term ranks, independent of batching and worker count. On the
  36M-triple s-bench file the store shrinks from 157 to 119 MiB (graph 127
  -> 93 MiB: term order compresses better than first-seen order;
  dictionary -15%, one id run per block) for 0.18 s of renumbering
  (sequential load 52.0 s either way, measured interleaved). A load into a
  non-empty store keeps first-seen ids.
- **Parallel N-Triples load** (`load::load_parallel`): worker threads parse
  line-aligned byte ranges and prepare (sort and group) their own batches.
  Into a non-empty store the calling thread interns them and collects
  triples, taking one batch per worker in turn. Arrival order would
  scatter related terms' ids (graph +6%, a 4-row chain 0.09 -> 6.4 ms on
  the s-bench file), which the term-order renumbering above undoes for
  fresh loads. Turtle can't be split at arbitrary lines (prefixes,
  multi-line statements) and stays sequential.
- **Sharded interning for fresh loads** (`load::load_sharded`): with one
  writer, interning was 75% of a 12-worker load (small.nt, 4.9M triples:
  writer interning 7.2 s, graph build 2.2 s, writer waiting on parsers
  0.09 s; 9.6 s in all vs 11.1 s with one worker). Into an empty
  dictionary, the loader now shards it by key range like s-bench's
  `nt_dict_file_bench` (bounds from a 64-range sample, 32 shards per
  writer dealt round-robin; as many writers as workers, since a worker
  waiting for its ids sleeps). Workers route each key, send each shard's
  sub-batch to its writer, wait for the ids and keep their triples with
  shard-encoded ids (`ShardRouter::global_id`). At the end s-dict's
  `concat_in_term_order` joins the shards' blocks, which already
  partition the keys in order, into one dictionary with term-rank ids
  without decoding a term, and the triples are remapped per worker in
  parallel. The result is the term-order dictionary a sequential load
  renumbers to, with the same ids. Building with `--features parallel`
  also makes s-graph's bulk build use rayon (2.2 -> 0.95 s on small.nt,
  identical tree). small.nt with 12 workers: 9.6 s -> 2.7 s (sample 0.04 s,
  parse + intern 1.75 s, join + remap 0.06 s, build 0.9 s); data.nt (19.7M
  triples): 92 s with one worker -> 18.3 s (intern 14 s, which grows
  faster than the data, as with one writer: inserts get dearer as blocks
  fill). 6 or 8 writers are within noise of 12; 4 add ~20%.
- **Trade-off**: fixed cost per graph (tree metadata + first block). Fine for
  datasets with a moderate number of graphs; "one graph per statement" data
  would want a different partitioning (e.g. graph id folded into a dimension).
- Terms are never removed from the dictionary when quads are deleted
  (s-dict reuses freed ids, and terms may be shared across graphs and
  properties). Garbage terms need a future compaction pass.

## Property graphs (experimental)

Properties are ordinary triples in the reserved graph
`<urn:x-s-store:properties>`: `(node, key, value)` for node properties and
`(<<( s p o )>>, key, value)` for edge properties, the edge identified by its
RDF 1.2 triple term in the same dictionary. One value per key. SPARQL reaches
them with an explicit `GRAPH <urn:x-s-store:properties>`; the graph is
excluded from `GRAPH ?g` and the dataset's named graphs, so property data
doesn't leak into RDF answers. Node labels and a Cypher mapping are open.

## SPARQL execution

**Plan** (`sparql::plan`): s-parser's algebra with variables mapped to row
slots and constants to term ids; flat `Copy`-node pools like the parser's.
Constants missing from the data get *query-local* ids
(`>= LOCAL_ID_BASE = 0xF000_0000`, reserved by capping the dictionary), and
so do computed terms (BIND, aggregates) — rows are plain `u32` throughout.

**Evaluation** (`sparql::exec`) is push-based backtracking over one
`&mut [u32]` row: an operator binds its slots, calls its continuation,
unbinds. `ControlFlow` lets LIMIT/ASK/EXISTS stop everything, including
s-graph's traversals.

**Delegation to s-graph** (`sparql::patterns`):
- A BGP over one graph → one `NativeJoin` (all patterns traversed jointly over
  the S/P/O trie, bindings streamed). Outer bindings are substituted as
  constants. A variable used both as predicate and subject/object becomes two
  join variables (dense vs dictionary id space) checked equal on output.
- A single pattern → one bounded `scan`.
- A BGP over a merged default graph (`FROM` several graphs) → pattern-at-a-time
  joins over merged scans (most-bound first); merged scans skip triples an
  earlier graph produced (RDF merge = set).
- `*`/`+`/`?` paths (and negated property sets inside them) → epsilon-free NFA
  → s-graph `PathAutomaton` (set semantics, distinct endpoints). The reversed
  automaton serves `?s path <o>`. Links, inverses, sequences and alternatives
  are rewritten to patterns/joins/unions per §18.2.2.4 (bag semantics).

**Join modes** (`compile::Analysis`): a join's right side runs once per left
solution with its bindings substituted (index nested loop — the cheap case
for s-graph's bound lookups) *only where substitution can't change the
result*: FILTER/BIND variables, OPTIONAL right sides and subquery outputs are
checked against the left side's certainly-bound variables. Otherwise the right
side is materialized once (per active graph), sorted on a key bound on both
sides, and probed by binary search.

**Batched existence checks** (`Node::Check`): a FILTER EXISTS / NOT EXISTS,
MINUS or join whose right side is a BGP with every variable certainly bound
by the left side is only point lookups once a left solution is substituted.
The compiler rewrites it to `Check`; the executor buffers left solutions
(4096 at a time, buffers pooled per query), probes all their patterns with
one s-graph `contains_batch` per graph, and passes the survivors on in their
original order. s-graph sorts the probes and shares descents, which matters
because one `contains` re-descends from the root, skipping siblings one by
one (µs per lookup on large trees). Measured ~10× over per-row evaluation.
Scans of a merged default graph check later graphs' triples against the
earlier graphs the same way, in batches after the first 64 triples (short
scans stay allocation-free). `QuadIndex::contains_batch_into` and
`Store::contains_batch` expose the same operation.

**Set joins** (`Node::SetJoin`): OPTIONAL (without a join condition),
FILTER EXISTS / NOT EXISTS and MINUS between two BGPs that share exactly one
variable, certainly bound by both, decide each left solution by that one
value. The compiler concatenates both BGPs; the executor runs them as one
native s-graph join, records the shared variable's values in an id set
(`sparql::idset`: one bit per dictionary id, zero pages until written,
cleared by touched words, pooled per query), passes the joined solutions on
for an OPTIONAL, then evaluates the left BGP once more and keeps, drops or
adds solutions by membership. Two s-graph traversals and no per-row lookup,
hashing or materialized right side: on a 20M-triple synthetic file the
benchmark's OPTIONAL / NOT EXISTS / MINUS went from 110-127 s (MINUS:
quadratic) to ~0.21 s, about the cost of the inner join plus a left scan.
The joined pass runs to completion before the left pass, so a LIMIT or ASK
over a set join still pays the whole join. Several shared variables keep
the earlier strategies (`Check`, bind or materialize).

**Counting** (`group::count_shape`): a GROUP over one triple pattern (no
repeated variable) whose aggregates are all `COUNT(*)`, `COUNT(DISTINCT *)`
or `COUNT(?v)` of a pattern variable, grouped by nothing or by the pattern's
predicate variable, produces no solutions: an all-variable pattern is the
graph's size, any other is s-graph's `count_matching` (leaf words counted by
popcount), and GROUP BY ?p counts once per dense predicate, skipping zero
counts. A merged default graph (shared triples would count twice) or an
already-bound grouping predicate falls back to ordinary grouping. On the
500k-subject synthetic file: COUNT of everything 318 ms → 0.01 ms, of one
class 18.5 → 0.75 ms, of the busiest predicate 195 → 68 ms, the predicate
histogram 352 → 234 ms (one traversal per predicate; a single traversal
counting all predicates at once would need s-graph support).

**Paths with both ends free** (`?x p* ?y`): only nodes with an edge
matching one of the automaton's start transitions can take a step, so walks
start from those alone, collected into an id set by s-graph's projection
`query_one_bound_into` (distinct subjects or objects of a predicate, deduped
in the descent). When the path matches zero-length paths, one scan per graph
puts every subject and object in a second id set, and each node that is not a
start pairs with itself only. Nodes come out in id order. The benchmark's
`subClassOf*` closure on the 500k-subject file went from 49 s (a walk, with
its µs-scale bound lookups, from each of ~1M nodes) to 0.3 s, mostly that
node scan.

**DISTINCT aggregates without GROUP BY** (`COUNT(DISTINCT ?v)`, ...) keep the
seen values in an id set rather than a hashed `RowSet`; query-local ids go to
a small `RowSet` aside. Ungrouped aggregation also no longer hashes the empty
group key per solution. `COUNT(DISTINCT ?s)` over every triple: 925 → 382 ms
on the 500k-subject file, now mostly s-graph's full scan.

**Expressions** use `Copy` values over one per-query text arena: constants
are decoded once into a frozen prefix; per-row scratch is truncated after each
evaluation. ORDER BY decodes each *distinct* key term once and sorts rows by
integer rank. DISTINCT/GROUP BY use a flat-buffer hash index (`RowSet`): no
per-row key allocation.

## Known gaps and upstream issues

- **s-parser, fixed in its working tree (uncommitted)**: nested function call
  argument lists were interleaved in the expression-list pool
  (`CONCAT("[", STR(?x), "]")` parsed as four arguments). Regression test
  added in `s-parser/tests/sparql.rs`.
- **s-parser, worked around here**: `Slice` is nested inside
  `Project`/`Distinct`, so `DISTINCT … LIMIT` would limit before dedup; the
  compiler reorders it.
- **s-parser**: the N-Triples tokenizer never yields a graph term (no
  N-Quads); the Turtle tokenizer discards `@base` (only a load-level base
  applies).
- Decimals are `f64` (no arbitrary precision); MD5/SHA*, SPARQL Update,
  SERVICE, RDF/XML and JSON-LD are not implemented; DESCRIBE returns the
  resource's outgoing triples (no blank-node closure).
- MINUS outside the set-join and check cases still materializes its right
  side without a key, so it compares every left row with every right row.
- Aggregates skip unbound inputs (Jena/RDF4J behaviour); bound values of the
  wrong type still make SUM/AVG an error.
- No persistence of the whole store yet (s-dict and s-graph can each
  serialize; the store needs a manifest with predicate table, id bound and
  graph list).
- No W3C SPARQL test-suite harness yet (needs SRX/SRJ result parsing);
  `s-bench/rdf-tests` has the suite.
