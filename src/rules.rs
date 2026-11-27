//! Rule sets: the built-in ones (rules/*.yml, embedded) and the user's
//! ($XDG_CONFIG_HOME/tinge/<set>.yml), which add sets or replace built-in ones.

use std::path::{Path, PathBuf};
use std::{env, fs, io};

include!(concat!(env!("OUT_DIR"), "/builtin.rs"));

/// The built-in set called `name`.
pub fn get(name: &str) -> Option<&'static [u8]> {
    BUILTIN.iter().find(|(n, _)| *n == name).map(|(_, data)| *data)
}

/// The built-in sets, by name.
pub fn names() -> Vec<&'static str> {
    BUILTIN.iter().map(|(n, _)| *n).collect()
}

/// Where the user's rule sets live: $XDG_CONFIG_HOME/tinge, by default
/// ~/.config/tinge (also on macOS).
pub fn user_dir() -> PathBuf {
    let base = match env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".config"),
    };
    base.join("tinge")
}

/// What `-c` names: a file to read, or a built-in set.
#[derive(Debug, PartialEq)]
pub enum Source {
    File(PathBuf),
    Builtin(&'static [u8]),
}

/// Resolves `-c`: a path (it has a '/' or a .yml / .yaml extension), or a set
/// name, read from `dir`/<name>.yml when that exists and built in otherwise.
pub fn find(spec: &str, dir: &Path) -> Result<Source, String> {
    if spec.contains('/') || spec.ends_with(".yml") || spec.ends_with(".yaml") {
        return Ok(Source::File(spec.into()));
    }
    for ext in ["yml", "yaml"] {
        let path = dir.join(format!("{spec}.{ext}"));
        if fs::metadata(&path).is_ok_and(|m| m.is_file()) {
            return Ok(Source::File(path));
        }
    }
    get(spec).map(Source::Builtin).ok_or_else(|| format!("no rule set {spec:?} (built in: {})", names().join(", ")))
}

/// Reads a set by name or path, the user's file before the built-in one.
pub fn read(spec: &str, dir: &Path) -> Result<Vec<u8>, ReadError> {
    match find(spec, dir).map_err(ReadError::Unknown)? {
        Source::Builtin(data) => Ok(data.to_vec()),
        Source::File(path) => fs::read(&path).map_err(|e| ReadError::Io(path, e)),
    }
}

#[derive(Debug)]
pub enum ReadError {
    Unknown(String),
    Io(PathBuf, io::Error),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ReadError::Unknown(msg) => f.write_str(msg),
            ReadError::Io(path, e) => match e.kind() {
                io::ErrorKind::NotFound => write!(f, "Configuration file {path:?} not found"),
                io::ErrorKind::PermissionDenied => {
                    write!(f, "Cannot read configuration file {path:?} (permission)")
                }
                _ => write!(f, "Cannot read configuration file {path:?}: {e}"),
            },
        }
    }
}

/// A fresh directory for a test, standing in for the user's rules directory.
#[cfg(test)]
pub(crate) fn tempdir() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = env::temp_dir().join(format!("tinge-test-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_resolves_names_and_paths() {
        let dir = tempdir();
        for spec in ["network", "server"] {
            assert!(matches!(find(spec, &dir), Ok(Source::Builtin(d)) if !d.is_empty()), "{spec}");
        }
        assert_eq!(find("x/rules.yml", &dir), Ok(Source::File("x/rules.yml".into())));
        assert_eq!(find("mine.yaml", &dir), Ok(Source::File("mine.yaml".into())));
        assert!(find("nope", &dir).unwrap_err().contains("network, server"));

        let own = dir.join("server.yml");
        fs::write(&own, "rules: []\n").unwrap();
        assert_eq!(find("server", &dir), Ok(Source::File(own)), "the user's set replaces the built-in one");
        assert!(matches!(find("network", &dir), Ok(Source::Builtin(_))));
        fs::remove_dir_all(dir).unwrap();
    }
}
