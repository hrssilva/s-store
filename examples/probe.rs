//! Times bound lookups of each shape against a loaded N-Triples file.
use s_parser::FileStream;
use s_store::Store;
use s_store::graph::GraphName;
use s_store::load::{Format, LoadOptions, load};
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("file.nt");
    let source = unsafe { FileStream::open(&path, 4 << 20).expect("open") };
    let mut store = Store::new();
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
}
