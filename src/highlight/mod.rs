//! Rule loading, matching and color insertion.

mod color;
mod load;
mod pattern;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayon::prelude::*;

pub use color::{Color, Type, rgb_to_xterm256};
use color::{Insert, NUM_TYPES, py_repr, strip_colors};
pub use pattern::{Pattern, compile};

/// The group a rule color applies to, as the rules file names it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Group {
    Number(i64),
    Name(String),
    Invalid,
}

/// Highlights the matches of a regex, optionally per group.
pub struct Rule {
    pub description: String,
    pub regex: String,
    pub exclusive: bool,
    pattern: Pattern,
    groups: Vec<usize>, // ascending
    colors: Vec<Color>,
}

impl Rule {
    /// Compiles `regex` and binds colors to groups.
    pub fn new(
        regex: &str,
        colors: HashMap<Group, Color>,
        description: String,
        exclusive: bool,
    ) -> Result<Rule, String> {
        let pattern = compile(regex).map_err(|e| format!("re.error: {e}"))?;
        let mut by_group: Vec<(usize, Color)> = Vec::new();
        for (key, color) in colors {
            let g = match key {
                Group::Number(n) => n,
                Group::Name(name) => match pattern.names.get(&name) {
                    Some(&n) => n as i64,
                    None => return Err(format!("named group {} not in regex", py_repr(&name))),
                },
                Group::Invalid => return Err("group must be an integer or a string".into()),
            };
            if g < 0 || g > pattern.groups as i64 {
                return Err(format!("regex has {} group(s); {g} is invalid", pattern.groups));
            }
            by_group.retain(|(n, _)| *n != g as usize);
            by_group.push((g as usize, color));
        }
        by_group.sort_by_key(|(g, _)| *g);
        let (groups, colors) = by_group.into_iter().unzip();
        Ok(Rule { description, regex: regex.to_owned(), exclusive, pattern, groups, colors })
    }

    fn matches(&self, index: usize, text: &[u8], out: &mut Vec<Match>) {
        for spans in self.pattern.find_all(text, &self.groups) {
            for (color, span) in spans.into_iter().enumerate() {
                // Ignore zero-length matches, like unmatched optional groups;
                // new lines in data can only come from exclusive rules
                if let Some((start, end)) = span
                    && start != end
                    && !text[start..end].contains(&b'\n')
                {
                    out.push(Match { start, end, rule: index, color });
                }
            }
        }
    }
}

/// A rule match: `rule` and `color` index the config's rules and that rule's
/// colors.
struct Match {
    start: usize,
    end: usize,
    rule: usize,
    color: usize,
}

/// The time spent and match count of one rule.
pub struct Benchmark {
    pub rule: Arc<Rule>,
    pub duration: Duration,
    pub count: usize,
}

/// A chunk with its colors stripped and its rule matches found: the
/// stateless, expensive part of highlighting.
struct Prepared {
    data: Vec<u8>,
    existing: Vec<Insert>,
    matches: Vec<Match>,
    took: Vec<Duration>,
}

/// An ordered set of rules plus the color state carried between chunks.
pub struct Config {
    rules: Vec<Arc<Rule>>,
    reset_codes: [Vec<u8>; NUM_TYPES],
    /// Rule usage, kept across reloads; `None` unless benchmarking.
    benchmark: Option<Vec<Benchmark>>,
}

impl Config {
    /// An empty config; `benchmark` records rule usage.
    pub fn new(benchmark: bool) -> Config {
        Config {
            rules: Vec::new(),
            reset_codes: Type::ALL.map(|t| t.reset().to_vec()),
            benchmark: benchmark.then(Vec::new),
        }
    }

    pub fn rules(&self) -> &[Arc<Rule>] {
        &self.rules
    }

    /// Replaces the rules, putting exclusive ones first (stable).
    pub fn set_rules(&mut self, mut rules: Vec<Arc<Rule>>) {
        rules.sort_by_key(|r| !r.exclusive);
        self.rules = rules;
    }

    /// Runs every rule over the text, in order. Exclusive rules (sorted
    /// first) blank what they match with new lines, which keeps the indices,
    /// so no later rule overlaps them.
    fn prepare(&self, data: &[u8]) -> Prepared {
        let (data, existing) = strip_colors(data);
        let mut text = data.clone();
        let mut matches = Vec::new();
        let mut took = Vec::new();
        for (i, rule) in self.rules.iter().enumerate() {
            let start = Instant::now();
            let from = matches.len();
            rule.matches(i, &text, &mut matches);
            if self.benchmark.is_some() {
                took.push(start.elapsed());
            }
            if rule.exclusive {
                for m in &matches[from..] {
                    text[m.start..m.end].fill(b'\n');
                }
            }
        }
        Prepared { data, existing, matches, took }
    }

    /// Returns data with the rules' colors inserted. Existing colors are
    /// stripped before matching and put back, so rules match the plain text.
    pub fn highlight(&mut self, data: &[u8]) -> Vec<u8> {
        let p = self.prepare(data);
        self.apply(p)
    }

    /// Highlights consecutive chunks, finding their matches in parallel; the
    /// result equals calling `highlight` on each in order.
    pub fn highlight_all(&mut self, chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        let preps: Vec<Prepared> = if chunks.len() < 2 {
            chunks.iter().map(|c| self.prepare(c)).collect()
        } else {
            let this = &*self;
            chunks.par_iter().map(|c| this.prepare(c)).collect()
        };
        preps.into_iter().map(|p| self.apply(p)).collect()
    }

    fn record(&mut self, p: &Prepared) {
        let Some(bench) = &mut self.benchmark else { return };
        let mut index = Vec::with_capacity(self.rules.len());
        for (i, rule) in self.rules.iter().enumerate() {
            let at = match bench.iter().position(|b| Arc::ptr_eq(&b.rule, rule)) {
                Some(at) => at,
                None => {
                    bench.push(Benchmark { rule: rule.clone(), duration: Duration::ZERO, count: 0 });
                    bench.len() - 1
                }
            };
            bench[at].duration += p.took[i];
            index.push(at);
        }
        for m in &p.matches {
            bench[index[m.rule]].count += 1;
        }
    }

    /// The codes to insert for the matches, merged into the existing ones;
    /// in descending position order.
    fn inserts(&self, matches: &[Match], mut inserts: Vec<Insert>) -> Vec<Insert> {
        for m in matches {
            let (mut start_index, mut end_index) = insert_index(m.start, m.end, &inserts);
            for code in &self.rules[m.rule].colors[m.color].codes {
                let typ = Some(code.typ);
                // The last color of this type before the match end (if any)
                // is what the match resets to
                let mut reset = self.reset_codes[code.typ.index()].clone();
                for ins in &inserts[end_index..] {
                    if ins.typ == typ {
                        reset = ins.code.clone();
                        break;
                    }
                    if ins.is_reset && ins.typ.is_none() {
                        reset = code.typ.reset().to_vec();
                        break;
                    }
                }
                let start_insert = Insert { pos: m.start, code: code.sgr.clone(), is_reset: false, typ };
                let mut end_insert = Insert { pos: m.end, code: reset, is_reset: true, typ };

                // Resets of this type inside the match would interrupt it;
                // turn them into this color, moving a full reset to the end
                for ins in &mut inserts[end_index..start_index] {
                    if ins.is_reset && (ins.typ == typ || ins.typ.is_none()) {
                        if ins.typ.is_none() {
                            end_insert.code = ins.code.clone();
                            end_insert.is_reset = ins.is_reset;
                            end_insert.typ = ins.typ;
                        }
                        ins.code = code.sgr.clone();
                        ins.is_reset = false;
                        ins.typ = typ;
                    }
                }
                inserts.insert(start_index, start_insert);
                inserts.insert(end_index, end_insert);
                start_index += 1;
                end_index += 1;
            }
        }
        inserts
    }

    /// Inserts the colors of a prepared chunk; it updates the color state
    /// carried to the next chunk, so chunks must be applied in order.
    fn apply(&mut self, p: Prepared) -> Vec<u8> {
        self.record(&p);
        let inserts = self.inserts(&p.matches, p.existing);
        if inserts.is_empty() {
            return p.data;
        }
        // The last color of each type in this chunk becomes the reset of the
        // next chunk; a full reset restores the defaults
        let mut to_update = [true; NUM_TYPES];
        for ins in &inserts {
            match ins.typ {
                None if ins.is_reset => {
                    for t in Type::ALL {
                        if to_update[t.index()] {
                            self.reset_codes[t.index()] = t.reset().to_vec();
                        }
                    }
                    break;
                }
                Some(t) if to_update[t.index()] => {
                    self.reset_codes[t.index()] = ins.code.clone();
                    to_update[t.index()] = false;
                    if !to_update.contains(&true) {
                        break;
                    }
                }
                _ => {}
            }
        }
        // Each code goes in at its position in the output so far, in list
        // order. Positions are mostly descending, so earlier codes don't move
        // later ones, and of the codes at one position the first listed ends
        // up last; then the output can be built in one pass. A position out
        // of order (stripping an SGR can join the bytes around it into
        // another, found next at a lower position) lands inside the codes
        // put in before it, which restores the original bytes.
        if !inserts.windows(2).all(|w| w[0].pos >= w[1].pos) {
            let mut out = p.data;
            for ins in &inserts {
                out.splice(ins.pos..ins.pos, ins.code.iter().copied());
            }
            return out;
        }
        let mut out = Vec::with_capacity(p.data.len() + inserts.iter().map(|i| i.code.len()).sum::<usize>());
        let mut last = 0;
        for ins in inserts.iter().rev() {
            out.extend_from_slice(&p.data[last..ins.pos]);
            out.extend_from_slice(&ins.code);
            last = ins.pos;
        }
        out.extend_from_slice(&p.data[last..]);
        out
    }

    /// Rule usage, most time spent first.
    pub fn benchmarks(&self) -> Vec<&Benchmark> {
        let mut out: Vec<&Benchmark> = self.benchmark.iter().flatten().collect();
        out.sort_by(|a, b| b.duration.cmp(&a.duration).then(b.count.cmp(&a.count)));
        out
    }
}

/// Highlighting serialized with reloads.
impl crate::stream::Highlighter for std::sync::Mutex<Config> {
    fn highlight(&self, data: &[u8]) -> Vec<u8> {
        self.lock().unwrap().highlight(data)
    }
    fn highlight_all(&self, chunks: &[&[u8]]) -> Vec<Vec<u8>> {
        self.lock().unwrap().highlight_all(chunks)
    }
}

/// Where a match's start and end inserts belong in the descending-ordered
/// inserts list.
fn insert_index(start: usize, end: usize, inserts: &[Insert]) -> (usize, usize) {
    let start_index = inserts.iter().position(|i| start >= i.pos).unwrap_or(inserts.len());
    // Overlapping matches put other colors between start and end, so the end
    // index is located independently
    let end_index = inserts.iter().position(|i| end > i.pos).unwrap_or(inserts.len());
    (start_index, end_index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(rules: &str) -> Config {
        let mut cfg = Config::new(false);
        let errs = cfg.load(rules.as_bytes(), false);
        assert!(errs.is_empty(), "{errs:?}");
        cfg
    }

    #[test]
    fn existing_colors_are_kept() {
        let mut cfg = config("rules:\n  - regex: up\n    color: bold\n");
        assert_eq!(cfg.highlight(b"\x1b[31mis up\x1b[0m"), b"\x1b[31mis \x1b[1mup\x1b[22m\x1b[0m");
    }

    #[test]
    fn joined_sgr_is_restored() {
        // Removing the inner SGR joins "\x1b" and "[39m" into another one
        let mut cfg = config("rules:\n  - regex: vlan\n    color: bold\n");
        let data = b"vlan 3\x1b\x1b[38;5;200m[39m0";
        assert_eq!(cfg.highlight(data), b"\x1b[1mvlan\x1b[22m 3\x1b\x1b[38;5;200m[39m0");
    }

    #[test]
    fn exclusive_rules_block_later_ones() {
        let mut cfg = config(
            "rules:\n  - regex: b\n    color: underline\n  - regex: abc\n    color: bold\n    exclusive: true\n",
        );
        assert_eq!(cfg.highlight(b"abc b"), b"\x1b[1mabc\x1b[22m \x1b[4mb\x1b[24m");
    }

    #[test]
    fn color_carries_to_the_next_chunk() {
        let mut cfg = config("rules:\n  - regex: up\n    color: bold\n");
        assert_eq!(cfg.highlight(b"\x1b[1mbold"), b"\x1b[1mbold");
        assert_eq!(cfg.highlight(b"up"), b"\x1b[1mup\x1b[1m");
    }

    #[test]
    fn highlight_all_equals_highlight_in_order() {
        let rules = rules_data("network");
        let corpus = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/corpus.bin")).unwrap();
        let chunks: Vec<&[u8]> = crate::stream::split(&corpus).iter().map(|c| c.data).collect();
        let mut one = Config::new(false);
        one.load(&rules, false);
        let mut all = Config::new(false);
        all.load(&rules, false);
        let each: Vec<Vec<u8>> = chunks.iter().map(|c| one.highlight(c)).collect();
        assert_eq!(all.highlight_all(&chunks), each);
    }

    #[test]
    fn benchmark_counts_matches() {
        let mut cfg = Config::new(true);
        cfg.load(b"rules:\n  - regex: up\n    color: bold\n", false);
        cfg.highlight_all(&[b"up up", b"down"]);
        let b = cfg.benchmarks();
        assert_eq!((b.len(), b[0].count), (1, 2));
    }

    fn rules_data(name: &str) -> Vec<u8> {
        crate::rules::get(name).unwrap().to_vec()
    }
}
