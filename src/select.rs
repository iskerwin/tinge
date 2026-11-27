//! Choosing the rule set when `-c` is not given: [`target_of`] takes the
//! program and host from the command line, [`select_set`] picks the set whose
//! `match` section applies.

use std::fs;
use std::path::Path;

use yaml_rust2::{Yaml, YamlLoader};

use crate::rules;

/// What a rule set's match section is checked against.
#[derive(Debug, Default, PartialEq)]
pub struct Target {
    /// The program name, such as ssh or tio.
    pub command: String,
    /// The host ssh / telnet connects to.
    pub host: String,
    /// From --tags.
    pub tags: Vec<String>,
}

/// ssh's options that take a value, so the destination is the first argument
/// that is neither one of them nor their value.
const SSH_ARG_OPTIONS: &str = "BbcDEeFIiJLlmOoPpQRSWw";

pub fn target_of(program: &[String], tags: &[String]) -> Target {
    let mut t = Target { tags: tags.to_vec(), ..Default::default() };
    let Some((first, args)) = program.split_first() else { return t };
    t.command = Path::new(first).file_name().map_or(first.clone(), |n| n.to_string_lossy().into_owned());
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            if let Some(h) = args.get(i + 1) {
                t.host = h.clone();
            }
            break;
        }
        if a.starts_with('-') && a.len() > 1 {
            // -p 22 takes the next argument, -p22 does not
            if t.command == "ssh" && a.len() == 2 && SSH_ARG_OPTIONS.contains(&a[1..]) {
                i += 1;
            }
            i += 1;
            continue;
        }
        t.host = a.clone();
        break;
    }
    let host = t.host.strip_prefix("ssh://").unwrap_or(&t.host);
    t.host = host.rsplit_once('@').map_or(host, |(_, h)| h).to_owned();
    t
}

/// A rule set's match section: it applies when any pattern (a
/// case-insensitive glob) matches; `default` marks the set used when none does.
#[derive(Debug, Default)]
struct Match {
    default: bool,
    commands: Vec<String>,
    hosts: Vec<String>,
    tags: Vec<String>,
}

fn strings(y: &Yaml) -> Vec<String> {
    match y {
        Yaml::Array(a) => a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    }
}

/// The match section of a rules file; a broken file is reported when it is
/// loaded, and matches nothing here.
fn match_of(data: &[u8]) -> Match {
    let Some(doc) = std::str::from_utf8(data)
        .ok()
        .and_then(|s| YamlLoader::load_from_str(s).ok())
        .and_then(|mut docs| (!docs.is_empty()).then(|| docs.swap_remove(0)))
    else {
        return Match::default();
    };
    let m = &doc["match"];
    Match {
        default: m["default"].as_bool().unwrap_or(false),
        commands: strings(&m["commands"]),
        hosts: strings(&m["hosts"]),
        tags: strings(&m["tags"]),
    }
}

/// Go's path.Match: `*` matches any run of characters but '/', `?` one
/// character but '/', `[...]` a class (`[^...]` negated), `\` escapes.
pub fn glob(pattern: &str, name: &str) -> bool {
    fn class(p: &[char], c: char) -> Option<(bool, usize)> {
        // Returns whether c is in the class at p[0] == '[' and its length
        let mut i = 1;
        let negated = p.get(i) == Some(&'^');
        if negated {
            i += 1;
        }
        let mut matched = false;
        let mut ranges = 0;
        loop {
            let &ch = p.get(i)?;
            if ch == ']' && ranges > 0 {
                return Some((matched != negated, i + 1));
            }
            if ch == ']' || ch == '-' {
                return None;
            }
            ranges += 1;
            let lo = if ch == '\\' {
                i += 1;
                *p.get(i)?
            } else {
                ch
            };
            i += 1;
            let mut hi = lo;
            if p.get(i) == Some(&'-') && p.get(i + 1).is_some_and(|&c| c != ']') {
                i += 1;
                hi = if p[i] == '\\' {
                    i += 1;
                    *p.get(i)?
                } else {
                    p[i]
                };
                i += 1;
            }
            matched |= lo <= c && c <= hi;
        }
    }
    fn go(p: &[char], n: &[char]) -> bool {
        match p.first() {
            None => n.is_empty(),
            Some('*') => (0..=n.len()).take_while(|&k| k == 0 || n[k - 1] != '/').any(|k| go(&p[1..], &n[k..])),
            Some(&c) => {
                let Some(&first) = n.first() else { return false };
                let (ok, len) = match c {
                    '?' => (first != '/', 1),
                    '[' => match class(p, first) {
                        Some(r) => r,
                        None => return false, // a broken pattern matches nothing
                    },
                    '\\' if p.len() > 1 => (p[1] == first, 2),
                    '\\' => return false,
                    _ => (c == first, 1),
                };
                ok && go(&p[len..], &n[1..])
            }
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    go(&p, &n)
}

fn glob_any(patterns: &[String], values: &[&str]) -> bool {
    patterns.iter().any(|p| {
        let p = p.to_lowercase();
        values.iter().any(|v| !v.is_empty() && glob(&p, &v.to_lowercase()))
    })
}

impl Match {
    fn applies(&self, t: &Target) -> bool {
        let tags: Vec<&str> = t.tags.iter().map(String::as_str).collect();
        glob_any(&self.commands, &[&t.command]) || glob_any(&self.hosts, &[&t.host]) || glob_any(&self.tags, &tags)
    }
}

/// The rule sets: the user's (`dir`/*.yml) and the built-in ones, by name.
fn set_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = rules::names().into_iter().map(str::to_owned).collect();
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            for ext in [".yml", ".yaml"] {
                if let Some(stem) = name.strip_suffix(ext) {
                    names.push(stem.to_owned());
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The rule set for `t`: the first, by name, whose match applies; else the
/// first marked default; else the built-in default.
pub fn select_set(t: &Target, dir: &Path) -> String {
    let mut fallback = None;
    for name in set_names(dir) {
        let Ok(data) = rules::read(&name, dir) else { continue };
        let m = match_of(&data);
        if m.applies(t) {
            return name;
        }
        if m.default && fallback.is_none() {
            fallback = Some(name);
        }
    }
    if let Some(name) = fallback {
        return name;
    }
    let names = rules::names();
    let builtin = names.iter().find(|n| match_of(rules::get(n).unwrap()).default);
    builtin.unwrap_or(&names[0]).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn target() {
        for (program, cmd, host) in [
            (&["ssh", "core-sw"][..], "ssh", "core-sw"),
            (&["/usr/bin/ssh", "-p", "2222", "-A", "admin@ruijie-lab", "show", "run"], "ssh", "ruijie-lab"),
            (&["ssh", "-p2222", "ssh://u@web01"], "ssh", "web01"),
            (&["ssh", "-o", "Port=22", "--", "web01"], "ssh", "web01"),
            (&["telnet", "10.0.0.2", "23"], "telnet", "10.0.0.2"),
            (&["tio", "-b", "9600", "/dev/cu.usbserial"], "tio", "9600"),
            (&[], "", ""),
        ] {
            let t = target_of(&args(program), &[]);
            assert_eq!((t.command.as_str(), t.host.as_str()), (cmd, host), "{program:?}");
        }
    }

    #[test]
    fn select() {
        let dir = rules::tempdir();
        let pick = |tags: &[&str], program: &[&str]| select_set(&target_of(&args(program), &args(tags)), &dir);
        for (tags, program, want) in [
            (&[][..], &["ssh", "ruijie-ZJZK-Core-SW"][..], "network"), // host pattern
            (&["ubuntu"], &["ssh", "web01"], "server"),                // nothing matches: the default
            (&["git", "Switch"], &["ssh", "sw1"], "network"),          // a tag, any case
            (&[], &["telnet", "10.0.0.2"], "network"),                 // the command
            (&[], &["tio", "/dev/cu.usbserial"], "network"),
            (&[], &[], "server"), // a pipe
        ] {
            assert_eq!(pick(tags, program), want, "{tags:?} {program:?}");
        }
        // A set of the user's own, chosen by its match; and a user default
        fs::write(dir.join("lab.yml"), "match:\n  hosts: ['lab-*']\nrules: []\n").unwrap();
        assert_eq!(pick(&[], &["ssh", "lab-01"]), "lab");
        fs::write(dir.join("alpha.yml"), "match:\n  default: true\nrules: []\n").unwrap();
        assert_eq!(pick(&[], &["ssh", "web01"]), "alpha", "a user default comes first by name");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn globs() {
        for (p, n, want) in [
            ("*", "abc", true),
            ("a*c", "abbbc", true),
            ("a*", "a/b", false),
            ("?b", "ab", true),
            ("[a-c]x", "bx", true),
            ("[^a-c]x", "bx", false),
            ("[]]", "]", false), // a bad pattern in Go too
            (r"\*", "*", true),
            (r"\*", "a", false),
            ("ruijie*", "ruijie-lab", true),
            ("*-sw*", "core-sw1", true),
            ("[a-", "a", false),
        ] {
            assert_eq!(glob(p, n), want, "{p:?} {n:?}");
        }
    }
}
