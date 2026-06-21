//! Search-latency benchmark. Indexes a repository in memory, then times the
//! search pipeline (the work the < 50 ms target is about — excludes process
//! startup).
//!
//!   cargo run --release --example bench -- /path/to/repo
//!
//! Defaults to the current directory.

use std::path::PathBuf;
use std::time::Instant;

use reference_query::index;
use reference_query::search;
use reference_query::store::Store;

const QUERIES: &[&str] = &[
    "user",
    "refund",
    "perform",
    "corpus",
    "parse",
    "normalize",
    "usr",
    "config",
    "client",
    "rp",
];

fn main() {
    let root = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));

    let mut store = Store::open_in_memory().expect("open store");
    let stats = index::index_path(&mut store, &root).expect("index");
    println!(
        "indexed {} symbols from {} file(s) under {}",
        stats.symbols,
        stats.files_indexed,
        root.display()
    );

    // warm up
    for q in QUERIES {
        let _ = search::search(&store, q, None, &search::ActiveFiles::default(), 10);
    }

    let mut times_us: Vec<u128> = Vec::new();
    for _ in 0..200 {
        for q in QUERIES {
            let start = Instant::now();
            let _ = search::search(&store, q, None, &search::ActiveFiles::default(), 10)
                .expect("search");
            times_us.push(start.elapsed().as_micros());
        }
    }
    times_us.sort_unstable();

    let pct = |p: f64| times_us[((times_us.len() as f64 - 1.0) * p).round() as usize];
    println!(
        "search over {} runs: p50 {} µs   p95 {} µs   max {} µs",
        times_us.len(),
        pct(0.50),
        pct(0.95),
        times_us[times_us.len() - 1],
    );
    let budget_us = 50_000;
    let over = times_us.iter().filter(|&&t| t > budget_us).count();
    println!("{}/{} runs exceeded the 50 ms budget", over, times_us.len());
}
