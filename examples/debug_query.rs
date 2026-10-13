//! Loads Turtle given on the command line and runs a query against it,
//! printing the plan and the solutions. A debugging aid.
use s_store::load::{Format, LoadOptions, load_slice};
use s_store::sparql::QueryOutput;

fn main() {
    let mut args = std::env::args().skip(1);
    let data = args.next().expect("turtle data");
    let query = args.next().expect("query");
    let mut store = s_store::Store::new();
    load_slice(&mut store, data.as_bytes(), Format::Turtle, &LoadOptions::default()).unwrap();
    println!("{:#?}", store.explain(&query).map(|p| p.nodes));
    match store.query(&query).unwrap() {
        QueryOutput::Solutions(s) => {
            for i in 0..s.len() {
                println!("{:?}", s.row(i).iter().map(|&id| s.key(&store, id)).collect::<Vec<_>>());
            }
        }
        other => println!("{other:?}"),
    }
}
