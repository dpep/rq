//! Search-latency benchmark: index a repository in memory, then time the search
//! pipeline (the work the < 50 ms target is about — excludes process startup).
//!
//! Not a correctness test, so it's `#[ignore]`d and a normal `cargo test` skips
//! it. Run it with:
//!
//!     make bench                        # this repo
//!     make bench REPO=/path/to/repo
//!
//! It lives in the lib rather than `examples/` because an example is a separate
//! crate: measuring `index`, `search` and `store` from there meant publishing
//! all three, which is a steep price for a benchmark. Measuring in-process is
//! the whole point, so driving the binary instead isn't an option.

use std::path::PathBuf;
use std::time::Instant;

use crate::index;
use crate::search;
use crate::store::Store;

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

const RUNS: usize = 200;
const BUDGET_US: u128 = 50_000;

#[test]
#[ignore = "benchmark, not a correctness test — run via `make bench`"]
fn search_latency() {
    let root = PathBuf::from(std::env::var("RQ_BENCH_REPO").unwrap_or_else(|_| ".".into()));

    let mut store = Store::open_in_memory().expect("open store");
    let stats = index::index_path(&mut store, &root).expect("index");
    println!(
        "indexed {} symbols from {} file(s) under {}",
        stats.symbols,
        stats.files_indexed,
        root.display()
    );

    let go = |q: &str| search::search(&store, q, None, None, &search::ActiveFiles::default(), 10);

    // warm up
    for q in QUERIES {
        let _ = go(q);
    }

    let mut times_us: Vec<u128> = Vec::new();
    for _ in 0..RUNS {
        for q in QUERIES {
            let start = Instant::now();
            let _ = go(q).expect("search");
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
    let over = times_us.iter().filter(|&&t| t > BUDGET_US).count();
    println!("{}/{} runs exceeded the 50 ms budget", over, times_us.len());
}
