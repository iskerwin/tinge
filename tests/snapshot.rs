//! Every built-in set over testdata/corpus.bin must give the bytes in
//! testdata/expected-<set>-<256|rgb>.bin. After an intended change, rewrite
//! them with `UPDATE=1 cargo test --test snapshot` and review the diff.

use std::fs;
use std::path::Path;

use tinge::highlight::Config;
use tinge::{rules, stream};

#[test]
fn snapshots() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let corpus = fs::read(dir.join("corpus.bin")).unwrap();
    let update = std::env::var_os("UPDATE").is_some_and(|v| !v.is_empty() && v != "0");
    let mut failed = Vec::new();
    for set in rules::names() {
        for mode in ["256", "rgb"] {
            let mut cfg = Config::new(false);
            let errs = cfg.load(rules::get(set).unwrap(), mode == "rgb");
            assert!(errs.is_empty(), "{set}: {errs:?}");
            let mut got = Vec::new();
            for c in stream::split(&corpus) {
                got.extend(cfg.highlight(c.data));
                got.extend_from_slice(c.sep);
            }
            let file = dir.join(format!("expected-{set}-{mode}.bin"));
            if update {
                fs::write(&file, &got).unwrap();
                continue;
            }
            let want = fs::read(&file).unwrap();
            if got != want {
                failed.push(format!("{set}-{mode}:\n{}", diff_lines(&corpus, &got, &want)));
            }
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// The first lines that differ, with their input.
fn diff_lines(corpus: &[u8], got: &[u8], want: &[u8]) -> String {
    let lines =
        |b: &[u8]| b.split(|&c| c == b'\n').map(|l| format!("{:?}", String::from_utf8_lossy(l))).collect::<Vec<_>>();
    let (input, got, want) = (lines(corpus), lines(got), lines(want));
    let mut out = String::new();
    let none = String::new();
    for i in (0..got.len().max(want.len())).filter(|&i| got.get(i) != want.get(i)).take(10) {
        out += &format!(
            "line {}\n input: {}\n   got: {}\n  want: {}\n",
            i + 1,
            input.get(i).unwrap_or(&none),
            got.get(i).unwrap_or(&none),
            want.get(i).unwrap_or(&none)
        );
    }
    out
}
