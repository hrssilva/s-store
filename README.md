# s-store

An RDF quad store with SPARQL 1.1 queries and experimental property-graph
support, built on the sibling libraries [`s-graph`](../s-graph) (compact
S/P/O trie, native joins, path automata), [`s-dict`](../s-dict) (term ↔ id
dictionary) and [`s-parser`](../s-parser) (N-Triples, Turtle, SPARQL).

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

The store is generic over s-graph's storage backends (`Heap` by default;
`File`/`Compressed` with the `file`/`compressed` features).

See [DESIGN.md](DESIGN.md) for the architecture, the reasoning behind it and
the known gaps.

## Testing

```sh
cargo test
cargo test --features file,compressed
cargo clippy --all-targets
cargo run --example debug_query -- '<turtle data>' '<query>'   # prints plan + rows
```
