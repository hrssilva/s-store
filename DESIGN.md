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
- Aggregates skip unbound inputs (Jena/RDF4J behaviour); bound values of the
  wrong type still make SUM/AVG an error.
- No persistence of the whole store yet (s-dict and s-graph can each
  serialize; the store needs a manifest with predicate table, id bound and
  graph list).
- No W3C SPARQL test-suite harness yet (needs SRX/SRJ result parsing);
  `s-bench/rdf-tests` has the suite.
