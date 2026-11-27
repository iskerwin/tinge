//! Rule regexes are Python `re` bytes patterns, matched against raw bytes:
//! every byte is one character and \d \w \s \b only know ASCII. PCRE2 without
//! UTF and UCP, with its default (C locale) tables, has exactly these
//! semantics, numbers named groups in order of appearance as Python does, and
//! supports lookbehind, `(?P<name>...)` and `(?P=name)`. The only translation
//! left is `\Z`: the absolute end in Python, `\z` in PCRE.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use pcre2::bytes::{CaptureLocations, Regex, RegexBuilder};
use thread_local::ThreadLocal;

/// A regex and a capture buffer per thread, reused across searches since
/// creating one allocates.
struct Engine {
    re: Regex,
    locs: ThreadLocal<RefCell<CaptureLocations>>,
}

impl Engine {
    fn new(re: Regex) -> Engine {
        Engine { re, locs: ThreadLocal::new() }
    }

    /// Adds the matches from `at` on to `all`; on a matching error, returns
    /// where the search stopped.
    fn find_from(&self, text: &[u8], mut at: usize, groups: &[usize], all: &mut Vec<Vec<Span>>) -> Result<(), usize> {
        let mut locs = self.locs.get_or(|| RefCell::new(self.re.capture_locations())).borrow_mut();
        while at <= text.len() {
            let m = match self.re.captures_read_at(&mut locs, text, at) {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(_) => return Err(at),
            };
            all.push(groups.iter().map(|&g| locs.get(g)).collect());
            at = if m.start() == m.end() { m.end() + 1 } else { m.end() };
        }
        Ok(())
    }
}

/// A compiled rule regex with Python-compatible group numbering.
pub struct Pattern {
    /// JIT-compiled where the platform has it: much faster, but its stack
    /// is small.
    jit: Engine,
    /// The interpreter, keeping its backtracking on the heap; made when the
    /// JIT one fails on some text.
    plain: OnceLock<Engine>,
    expr: String,
    /// Number of capturing groups (group 0 not included).
    pub groups: usize,
    pub names: HashMap<String, usize>,
}

fn translate(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        out.push(c);
        if c == '\\' {
            match chars.next() {
                Some('Z') => out.push('z'),
                Some(e) => out.push(e),
                None => {}
            }
        }
    }
    out
}

/// Compiles a rule regex (a Python bytes pattern).
pub fn compile(pattern: &str) -> Result<Pattern, String> {
    let expr = translate(pattern);
    let re = RegexBuilder::new().jit_if_available(true).build(&expr).map_err(|e| e.to_string())?;
    let names = re.capture_names().iter().enumerate().filter_map(|(i, n)| n.as_ref().map(|n| (n.clone(), i))).collect();
    Ok(Pattern { groups: re.captures_len() - 1, names, jit: Engine::new(re), plain: OnceLock::new(), expr })
}

/// A half-open byte range; `None` for a group that did not participate.
pub type Span = Option<(usize, usize)>;

impl Pattern {
    /// For every match, the spans of the requested groups. After an empty
    /// match the next search starts one byte later. When the JIT stack runs
    /// out, the interpreter carries on; an error there too (the backtracking
    /// limit) ends the search.
    pub fn find_all(&self, text: &[u8], groups: &[usize]) -> Vec<Vec<Span>> {
        let mut all = Vec::new();
        if let Err(at) = self.jit.find_from(text, 0, groups, &mut all) {
            let plain = self
                .plain
                .get_or_init(|| Engine::new(RegexBuilder::new().build(&self.expr).expect("compiled with JIT before")));
            let _ = plain.find_from(text, at, groups, &mut all);
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_semantics() {
        for (pattern, data, group, want) in [
            (r"\bup\b", "up，", 0, Some("up")),       // \b after ASCII before UTF-8
            (r"\w+", "é1a", 0, Some("1a")),           // \w is ASCII
            (r"\d+", "٣12", 0, Some("12")),           // \d is ASCII
            (r"a\sb", "a\x0bb", 0, Some("a\x0bb")),   // \s includes \v
            (r"a\sb", "a\u{a0}b", 0, None),           // but no NBSP
            (r"(?P<x>a)(b)", "ab", 2, Some("b")),     // groups numbered in order
            (r"(?P<x>a)(?P=x)", "aa", 0, Some("aa")), // named backreference
            (r"\xc3\xa9", "é", 0, Some("é")),         // byte escapes match bytes
            (r"[]a]+", "]a]", 0, Some("]a]")),        // leading ] is literal
            (r"x$", "x", 0, Some("x")),
            (r"a\Z", "a\n", 0, None),                      // \Z is the absolute end
            (r"(?<=^\[\d\] )up", "[1] up", 0, Some("up")), // lookbehind
            (r"(?i)gigabit", "GIGABIT", 0, Some("GIGABIT")),
            (r"(?i)é", "É", 0, None), // case folding is ASCII only
        ] {
            let p = compile(pattern).unwrap_or_else(|e| panic!("{pattern}: {e}"));
            let found = p.find_all(data.as_bytes(), &[group]);
            let got = found.first().and_then(|spans| spans[0]).map(|(s, e)| &data.as_bytes()[s..e]);
            assert_eq!(got, want.map(str::as_bytes), "{pattern:?} on {data:?}");
        }
        // One byte per character
        let p = compile(".{2}").unwrap();
        assert_eq!(p.find_all("中".as_bytes(), &[0])[0][0], Some((0, 2)));
        // Invalid UTF-8 is matched byte by byte
        let p = compile(r"up\b").unwrap();
        assert_eq!(p.find_all(b"\xffup\xfe", &[0])[0][0], Some((1, 3)));
    }

    #[test]
    fn deep_backtracking_outgrows_the_jit_stack() {
        // Each repetition of a capturing group with alternatives keeps a
        // backtracking frame: far more than the JIT's 32 KiB stack holds
        let p = compile("(?:(a)|b)*c").unwrap();
        let text = [b"ab".repeat(50_000), b"c".to_vec()].concat();
        let jit = p.jit.re.captures_read_at(&mut p.jit.re.capture_locations(), &text, 0);
        assert!(jit.is_err(), "the JIT copes; make the text longer");
        assert_eq!(p.find_all(&text, &[0])[0][0], Some((0, text.len())));
    }

    #[test]
    fn empty_matches_advance() {
        let p = compile("a*").unwrap();
        let spans: Vec<Span> = p.find_all(b"baa", &[0]).into_iter().map(|s| s[0]).collect();
        assert_eq!(spans, [Some((0, 0)), Some((1, 3)), Some((3, 3))]);
    }

    #[test]
    fn groups_and_names() {
        let p = compile(r"(?P<a>x)(y)(?<b>z)").unwrap();
        assert_eq!(p.groups, 3);
        assert_eq!(p.names["a"], 1);
        assert_eq!(p.names["b"], 3);
        assert!(compile("(").is_err());
        assert!(compile("(?P<a>x)(?P<a>y)").is_err());
    }
}
