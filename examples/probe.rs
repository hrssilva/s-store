//! Times bound lookups of each shape against a loaded N-Triples file.
use s_parser::FileStream;
use s_store::Store;
use s_store::graph::GraphName;
use s_store::load::{Format, LoadOptions, load};
use s_graph::automata::{Bgp, Search, Term};
use std::ops::ControlFlow;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("file.nt");
    let source = unsafe { FileStream::open(&path, 4 << 20).expect("open") };
    let mut store: Store = Store::with_config(s_store::store::StoreConfig { block_capacity: std::env::var("BLOCK").ok().and_then(|b| b.parse().ok()).unwrap_or(s_graph::DEFAULT_BLOCK_CAPACITY) });
    load(&mut store, source, Format::NTriples, &LoadOptions::default()).expect("load");
    store.shrink_to_fit();
    let tree = store.index().partition(GraphName::Default).unwrap().tree();
    println!("height {} domains {:?} bytes {}", tree.height(), (0..3).map(|d| tree.domain_size(d)).collect::<Vec<_>>(), tree.mem_size_bytes());
    let all: Vec<_> = tree.scan([None; 3]).collect();
    let sample: Vec<_> = (0..20_000).map(|i| all[(i * 7919 * 13) % all.len()]).collect();
    let shapes: [(&str, [bool; 3]); 7] = [("s p ?", [true, true, false]), ("s ? ?", [true, false, false]), ("s ? o", [true, false, true]), ("? p o", [false, true, true]), ("? ? o", [false, false, true]), ("s p o", [true, true, true]), ("? p ?", [false, true, false])];
    for (name, mask) in shapes {
        let n = if mask == [false, true, false] { 20 } else { sample.len() };
        let t = Instant::now();
        let mut rows = 0usize;
        for &(s, p, o) in &sample[..n] {
            let b = [mask[0].then_some(s), mask[1].then_some(p), mask[2].then_some(o)];
            rows += tree.scan(b).count();
        }
        let e = t.elapsed();
        println!("{name}: {:>8.2} us/lookup, {:>10.1} rows/lookup, {:>7.1} ns/row", e.as_secs_f64() * 1e6 / n as f64, rows as f64 / n as f64, e.as_secs_f64() * 1e9 / rows as f64);
    }
    let t = Instant::now();
    let hits = sample.iter().filter(|&&(s, p, o)| tree.contains(s, p, o)).count();
    println!("contains: {:.2} us ({hits} hits)", t.elapsed().as_secs_f64() * 1e6 / sample.len() as f64);
    let t = Instant::now();
    let n = tree.scan([None; 3]).count();
    println!("full scan: {:.1} ns/triple ({n})", t.elapsed().as_secs_f64() * 1e9 / n as f64);
    // Scans vs one-atom automaton searches, compiled once per shape and
    // run with the bound positions as parameters.
    for (name, mask) in [("s p ?", [true, true, false]), ("s ? o", [true, false, true]), ("? p o", [false, true, true]), ("s ? ?", [true, false, false])] {
        let bounds = |&(s, p, o): &(u32, u32, u32)| [mask[0].then_some(s), mask[1].then_some(p), mask[2].then_some(o)];
        let n = sample.len().min(5000);
        let t = Instant::now();
        let rows: usize = sample[..n].iter().map(|x| tree.scan(bounds(x)).count()).sum();
        let scan = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        let machine = Bgp::new(3).triple([Term::Var(0), Term::Var(1), Term::Var(2)]);
        let search = tree.search(&machine).expect("compile");
        let (mut ws, mut seen) = (Default::default(), 0usize);
        let t = Instant::now();
        for x in &sample[..n] {
            let b = bounds(x);
            let values = b.map(|v| v.unwrap_or(0));
            let bound = [b.iter().enumerate().fold(0u64, |m, (i, v)| m | (u64::from(v.is_some()) << i))];
            let _: ControlFlow<()> = search.visit_matching(&mut ws, s_graph::automata::Binding { values: &values, bound: &bound }, |_| {
                seen += 1;
                ControlFlow::Continue(())
            });
        }
        let automaton = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        println!("{name}: scan {scan:>8.2} us  parameterized search {automaton:>8.2} us  ({:.1} rows, {:.1} found)", rows as f64 / n as f64, seen as f64 / n as f64);
    }
}
