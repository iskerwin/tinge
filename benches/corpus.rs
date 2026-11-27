//! Times the built-in network rules over testdata/corpus.bin, chunk by chunk
//! (`highlight`) and in one batch (`highlight_all`): cargo bench.

use std::hint::black_box;
use std::time::{Duration, Instant};

use tinge::highlight::Config;
use tinge::{rules, stream};

fn time(name: &str, mut f: impl FnMut()) {
    f(); // warm up
    let start = Instant::now();
    let mut runs = 0;
    while start.elapsed() < Duration::from_secs(3) {
        f();
        runs += 1;
    }
    println!("{name:<14} {:>10.3} ms/op  ({runs} runs)", start.elapsed().as_secs_f64() * 1e3 / f64::from(runs));
}

fn main() {
    let corpus = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/corpus.bin")).unwrap();
    let chunks: Vec<&[u8]> = stream::split(&corpus).iter().map(|c| c.data).collect();
    let mut cfg = Config::new(false);
    cfg.load(rules::get("network").unwrap(), false);
    time("corpus", || {
        for c in &chunks {
            black_box(cfg.highlight(c));
        }
    });
    time("corpus_all", || {
        black_box(cfg.highlight_all(&chunks));
    });
}
