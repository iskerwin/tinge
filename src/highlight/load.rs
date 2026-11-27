//! Loading a YAML rules file.

use std::collections::HashMap;
use std::sync::Arc;

use yaml_rust2::{Yaml, YamlLoader};

use super::color::{Color, Palette, add_color, parse_color, py_repr};
use super::{Config, Group, Rule};

/// How Go's fmt.Sprint shows a value, for messages.
fn show(y: &Yaml) -> String {
    match y {
        Yaml::String(s) | Yaml::Real(s) => s.clone(),
        Yaml::Integer(n) => n.to_string(),
        Yaml::Boolean(b) => b.to_string(),
        Yaml::Array(a) => format!("[{}]", a.iter().map(show).collect::<Vec<_>>().join(" ")),
        Yaml::Hash(h) => {
            format!("map[{}]", h.iter().map(|(k, v)| format!("{}:{}", show(k), show(v))).collect::<Vec<_>>().join(" "))
        }
        Yaml::Null | Yaml::BadValue | Yaml::Alias(_) => "<nil>".into(),
    }
}

impl Config {
    /// Parses a YAML rules file. On a parse error nothing is loaded and the
    /// rules stay as they were; invalid palette colors abort the load, while
    /// invalid rules are skipped. Every problem is returned as a message.
    pub fn load(&mut self, data: &[u8], rgb: bool) -> Vec<String> {
        let docs = std::str::from_utf8(data)
            .map_err(|e| e.to_string())
            .and_then(|s| YamlLoader::load_from_str(s).map_err(|e| e.to_string()));
        let root = match docs {
            Ok(mut docs) if !docs.is_empty() => docs.swap_remove(0),
            Ok(_) => Yaml::Null,
            Err(e) => return vec![format!("Parse error: {e}")],
        };

        let mut palette = None;
        match &root["palette"] {
            Yaml::BadValue | Yaml::Null => {}
            Yaml::Hash(entries) => {
                let mut p = Palette::new();
                for (name, value) in entries {
                    let result = match value {
                        Yaml::String(s) => add_color(&mut p, &show(name), s),
                        _ => Err("color value must be a string".into()),
                    };
                    if let Err(e) = result {
                        return vec![format!(
                            "Error on palette color {}: {}: {e}",
                            py_repr(&show(name)),
                            py_repr(&show(value))
                        )];
                    }
                }
                palette = Some(p);
            }
            _ => return vec![r#"Parse error: "palette" is not a dictionary"#.into()],
        }

        let list = match &root["rules"] {
            Yaml::BadValue | Yaml::Null => {
                return vec![r#"Parse error: "rules" list not found in configuration"#.into()];
            }
            Yaml::Array(list) => list,
            _ => return vec![r#"Parse error: "rules" is not a list"#.into()],
        };

        let mut errs = Vec::new();
        let mut rules = Vec::new();
        for item in list {
            match parse_rule(item, palette.as_ref(), rgb) {
                Ok(rule) => rules.push(Arc::new(rule)),
                Err(e) => errs.push(e),
            }
        }
        self.set_rules(rules);
        errs
    }
}

fn parse_rule(item: &Yaml, palette: Option<&Palette>, rgb: bool) -> Result<Rule, String> {
    if !matches!(item, Yaml::Hash(_)) {
        return Err(format!("Rule {} not a dictionary", show(item)));
    }
    let regex = &item["regex"];
    let truncated: String = show(regex).chars().take(30).collect();
    let mut label = format!("Rule(regex={})", py_repr(&truncated));
    let mut description = String::new();
    match &item["description"] {
        Yaml::BadValue | Yaml::Null => {}
        Yaml::String(s) => {
            description = s.clone();
            if !s.is_empty() {
                label.pop();
                label += &format!(", description={})", py_repr(s));
            }
        }
        _ => return Err(format!("Error on {label}: description must be a string")),
    }
    let fail = |e: String| Err(format!("Error on {label}: {e}"));

    let exclusive = match &item["exclusive"] {
        Yaml::BadValue => false,
        Yaml::Boolean(b) => *b,
        _ => return fail("exclusive must be a boolean".into()),
    };
    let Yaml::String(regex) = regex else {
        return fail("regex must be a string".into());
    };

    let specs: Vec<(&Yaml, &Yaml)> = match &item["color"] {
        Yaml::Hash(h) => h.iter().collect(),
        other => vec![(&Yaml::Integer(0), other)],
    };
    let mut colors: HashMap<Group, Color> = HashMap::new();
    for (group, spec) in specs {
        let Yaml::String(spec) = spec else {
            return fail("color must be a string".into());
        };
        let group = match group {
            Yaml::Integer(n) => Group::Number(*n),
            Yaml::String(s) => Group::Name(s.clone()),
            _ => Group::Invalid,
        };
        match parse_color(spec, palette, rgb) {
            Ok(color) => colors.insert(group, color),
            Err(e) => return fail(e),
        };
    }
    match Rule::new(regex, colors, description, exclusive) {
        Ok(rule) => Ok(rule),
        Err(e) => fail(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_reports_errors() {
        let mut cfg = Config::new(false);
        let errs = cfg.load(
            br##"
palette:
  red: "#ff0000"
rules:
  - regex: ok
    color: f.red
  - regex: '('
    color: bold
  - regex: x
    color: f.blue
  - regex: x(y)
    color: {2: bold}
  - regex: x
    color: f#12
  - regex: x
    exclusive: maybe
    color: bold
  - regex: (?P<n>x)
    color: {m: bold}
  - "not a rule"
"##,
            false,
        );
        let want = [
            "re.error",
            "color 'blue' not in palette",
            "regex has 1 group(s); 2 is invalid",
            "invalid color format 'f#12'",
            "exclusive must be a boolean",
            "named group 'm' not in regex",
            "not a dictionary",
        ];
        assert_eq!(errs.len(), want.len(), "{errs:?}");
        for (e, w) in errs.iter().zip(want) {
            assert!(e.contains(w), "{e:?} lacks {w:?}");
        }
        assert_eq!(cfg.rules().len(), 1);
        let errs = cfg.load(b"rules: [\n", false);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(cfg.rules().len(), 1, "a parse error must keep the rules");
    }

    #[test]
    fn named_and_numbered_groups() {
        let mut cfg = Config::new(false);
        let errs = cfg.load(b"rules:\n  - regex: '(?P<a>x)(y)'\n    color: {a: bold, 2: underline}\n", false);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.highlight(b"xy"), b"\x1b[1mx\x1b[22m\x1b[4my\x1b[24m");
    }
}
