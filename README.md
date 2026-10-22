# s-store

An RDF quad store with SPARQL 1.1 queries and experimental property-graph
support, built on the libraries [`s-graph`](https://github.com/hrssilva/s-graph) (compressed
S/P/O FSTs searched by automata), [`s-dict`](https://github.com/hrssilva/s-dict) (term ↔ id
dictionary) and [`s-parser`](https://github.com/hrssilva/s-parser) (N-Triples, Turtle, SPARQL).

```rust
use s_store::load::{Format, LoadOptions, load_slice};
use s_store::sparql::QueryOutput;
use s_store::Store;

let mut store = Store::new();
load_slice(&mut store, b"<http://a> <http://knows> <http://b> .\n", Format::NTriples, &LoadOptions::default())?;
if let QueryOutput::Solutions(rows) = store.query("SELECT ?x WHERE { <http://a> <http://knows>+ ?x }")? {
    for i in 0..rows.len() {
        println!("{:?}", rows.key(&store, rows.row(i)[0]));
    }
}
# Ok::<(), s_store::StoreError>(())
```

Terms are passed and returned as canonical N-Triples keys (`<iri>`, `"lit"@en`,
`_:b`, ...). Named graphs: `LoadOptions { graph: GraphRef::Named("<http://g>"), .. }`.
Property graphs: `store.properties_mut().set(Owner::Edge(s, p, o), key, value)`.

Each graph is one s-graph FST, and SPARQL BGPs run as s-graph automata over
it. The store is generic over s-graph's block storage (heap by default; a
temporary file or LZ4-compressed blocks with the `file`/`compressed`
features). Inserts into a non-empty graph are FST edits held in memory;
`Store::seal` writes them into blocks beside the FST in time proportional to
the edits, and `Store::shrink_to_fit` repacks the whole FST, reclaiming the
records edits made obsolete.

See [DESIGN.md](DESIGN.md) for the architecture, the reasoning behind it and
the known gaps.

## HTTP endpoint

`examples/server.rs` serves a store over HTTP (standard library only):

```sh
cargo run --release --example server -- [--bind 127.0.0.1:7878] [--workers N] [FILE[=GRAPH]]...
# e.g. data.nt into the default graph, more.ttl into a named graph:
cargo run --release --example server -- data.nt more.ttl=http://ex/g1
```

Files (`.nt`, `.ttl`) are loaded before serving, read through memory-mapped
windows. N-Triples files are parsed by `--workers` threads (default: the
available cores), and into an empty store also interned by as many
dictionary writer threads; Turtle files load on one thread. Then open
`http://127.0.0.1:7878/` for a page that runs queries and uploads files, or
use the endpoints directly:

| Endpoint | |
|---|---|
| `GET /sparql?query=...`, `POST /sparql` | Query in the URL, a form (`query=...`) or the body (`Content-Type: application/sparql-query`). SELECT/ASK answer as SPARQL JSON results, or TSV with `format=tsv` / `Accept: text/tab-separated-values`; CONSTRUCT/DESCRIBE as N-Triples. |
| `POST /load?format=nt\|ttl[&graph=IRI][&base=IRI]` | Loads the request body into the default graph or `graph`. Into a non-empty graph the triples are FST edits, sealed into blocks after the upload. |
| `POST /compact` | Repacks every graph's FST, reclaiming the space uploads left behind. It rewrites each graph (about 0.15 s per 5M triples), so run it after a burst of uploads. |
| `GET /stats` | Triple count and memory use. |

```sh
curl --get localhost:7878/sparql --data-urlencode 'query=SELECT * WHERE { ?s ?p ?o } LIMIT 10'
curl -H 'Accept: text/tab-separated-values' --data-urlencode 'query=SELECT ?p (COUNT(*) AS ?n) WHERE { ?s ?p ?o } GROUP BY ?p' localhost:7878/sparql
curl -X POST --data-binary @file.ttl 'localhost:7878/load?format=ttl&graph=http://ex/g1'
curl -X POST localhost:7878/compact
```

Queries run concurrently; a load waits for running queries and holds off new
ones. Each query's and load's time is logged to stderr. It is a development
tool: no authentication (it binds to localhost unless `--bind` says
otherwise), uploads are held in memory (up to 1 GiB; load large files as
arguments instead), request bodies need a `Content-Length` (no chunked
encoding), and each connection serves one request.

## Testing

```sh
cargo test
cargo test --features file,compressed
cargo clippy --all-targets
cargo run --example debug_query -- '<turtle data>' '<query>'   # prints plan + rows
```
