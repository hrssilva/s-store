//! Times bound lookups of each shape against a loaded N-Triples file.
use s_parser::FileStream;
use s_store::Store;
use s_store::graph::GraphName;
use s_store::load::{Format, LoadOptions, load};
use s_graph::tree::DimRestriction;
use std::ops::ControlFlow;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("file.nt");
    let source = unsafe { FileStream::open(&path, 4 << 20).expect("open") };
    let mut store: Store = Store::with_config(s_store::store::StoreConfig { block_capacity: std::env::var("BLOCK").ok().and_then(|b| b.parse().ok()).unwrap_or(s_graph::DEFAULT_BLOCK_CAPACITY) });
    load(&mut store, source, Format::NTriples, &LoadOptions::default()).expect("load");
    store.shrink_to_fit();
    let tree = store.index().partition(GraphName::Default).unwrap().tree();
    println!("height {} domains {:?} blocks {}", tree.height(), (0..3).map(|d| tree.domain_size(d)).collect::<Vec<_>>(), tree.block_count());
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
    // restricted_descent (sorted, buffered) and its streaming visit vs scan.
    let mut buf = Vec::new();
    let r = |v: [Option<u32>; 3]| v.map(|b| b.map_or(DimRestriction::Free, DimRestriction::Exact));
    for (name, mask, free) in [("s p ?", [true, true, false], 2usize), ("s ? o", [true, false, true], 1), ("? p o", [false, true, true], 0), ("s ? ?", [true, false, false], 2)] {
        let bounds = |&(s, p, o): &(u32, u32, u32)| [mask[0].then_some(s), mask[1].then_some(p), mask[2].then_some(o)];
        let n = sample.len().min(5000);
        let t = Instant::now();
        let rows: usize = sample[..n].iter().map(|x| tree.scan(bounds(x)).count()).sum();
        let scan = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        let t = Instant::now();
        for x in &sample[..n] {
            tree.restricted_descent_into(r(bounds(x)), free, &mut buf);
        }
        let into = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        let t = Instant::now();
        let mut seen = 0usize;
        for x in &sample[..n] {
            let _: ControlFlow<()> = tree.restricted_descent_visit(r(bounds(x)), free, |_| {
                seen += 1;
                ControlFlow::Continue(())
            });
        }
        let visit = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        println!("{name}: scan {scan:>8.2} us  descent_into {into:>8.2} us  descent_visit {visit:>8.2} us  ({:.1} rows, {:.1} visited)", rows as f64 / n as f64, seen as f64 / n as f64);
    }
    for p in 0..tree.domain_size(1).min(8) as u32 {
        let t = Instant::now();
        let mut ends = 0usize;
        let _: ControlFlow<()> = tree.restricted_descent_visit([DimRestriction::Free, DimRestriction::Exact(p), DimRestriction::Free], 0, |_| {
            ends += 1;
            ControlFlow::Continue(())
        });
        let a = t.elapsed();
        let t = Instant::now();
        let n = tree.scan([None, Some(p), None]).count();
        println!("subjects of p{p}: descent_visit {a:.2?} ({ends} emitted) vs scan {:.2?} ({n} triples)", t.elapsed());
    }
}
