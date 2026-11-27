//! tinge highlights terminal output with regex rules. It runs a program in a
//! pseudo-terminal (or reads stdin) and colors what it prints.

mod pty;

use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::ManuallyDrop;
use std::os::fd::FromRawFd;
use std::process::{Command, ExitCode};
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::{Receiver, bounded};
use signal_hook::consts::{SIGINT, SIGUSR1};
use signal_hook::iterator::Signals;
use tinge::highlight::{Benchmark, Config};
use tinge::{rules, select, stream};

const USAGE: &str = "usage: tinge [-h] [-b] [-c rules] [-t tags] [-r] [-R] [-v] [--print-rules] [program ...]

Highlight the output of a program (or of stdin) with regex rules.

positional arguments:
  program ...         run a program with anything after it used as arguments

options:
  -h, --help          show this help message and exit
  -b, --benchmark     at exit, print rule usage statistics
  -c, --config rules  a rule set by name (built in: network, server) or a file
                      path; without it, the set whose match section fits the
                      program, its host or --tags. ~/.config/tinge/<set>.yml
                      adds a set, or replaces the built-in one of that name
  -t, --tags tags     the host's tags (comma separated) for choosing the set
  -r, --reload        reload the config of all tinge instances
  -R, --rgb           use RGB colors (default: detect support, fallback to xterm-256)
  -v, --version       show program's version number and exit
  --print-rules       print the rule set that would be used (its name on stderr)
";

#[derive(Debug, Default, PartialEq)]
struct Options {
    benchmark: bool,
    reload: bool,
    rgb: bool,
    print: bool,
    help: bool,
    version: bool,
    config: String,
    tags: Vec<String>,
    program: Vec<String>,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut o = Options::default();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            o.program = args[i + 1..].to_vec();
            break;
        }
        if !a.starts_with('-') || a == "-" {
            o.program = args[i..].to_vec();
            break;
        }
        let (name, value) = match a.split_once('=') {
            Some((n, v)) => (n, Some(v.to_owned())),
            None => (a.as_str(), None),
        };
        let mut value_of = |long: &str| match value.clone() {
            Some(v) => Ok(v),
            None => {
                i += 1;
                args.get(i).cloned().ok_or(format!("argument {long}: expected one argument"))
            }
        };
        match name {
            "-h" | "--help" => o.help = true,
            "--print-rules" => o.print = true,
            "-v" | "--version" => o.version = true,
            "-b" | "--benchmark" => o.benchmark = true,
            "-r" | "--reload" => o.reload = true,
            "-R" | "--rgb" => o.rgb = true,
            "-t" | "--tags" => {
                let v = value_of("-t/--tags")?;
                o.tags.extend(v.split([',', ' ']).filter(|t| !t.is_empty()).map(str::to_owned));
            }
            "-c" | "--config" => o.config = value_of("-c/--config")?,
            // Combined short flags, like -bR
            _ if a.len() > 2 && !a[1..].starts_with('-') && a[1..].chars().all(|c| "bRr".contains(c)) => {
                o.benchmark |= a.contains('b');
                o.rgb |= a.contains('R');
                o.reload |= a.contains('r');
            }
            _ => return Err(format!("unrecognized arguments: {a}")),
        }
        if o.help || o.version {
            break;
        }
        i += 1;
    }
    Ok(o)
}

/// Writes a message to stderr; \r\n because the terminal may be raw.
fn eprint(msg: &str) {
    let _ = write!(io::stderr(), "tinge: {msg}\r\n");
}

/// Reads the rules `-c` names; problems are printed and give `None`.
fn read_rules(spec: &str) -> Option<Vec<u8>> {
    rules::read(spec, &rules::user_dir()).map_err(|e| eprint(&e.to_string())).ok()
}

/// Replaces the rules with the ones `-c` names, read again from disk.
fn load(cfg: &Mutex<Config>, spec: &str, rgb: bool) {
    let Some(data) = read_rules(spec).filter(|d| !d.is_empty()) else { return };
    let errs = cfg.lock().unwrap().load(&data, rgb);
    for e in errs {
        eprint(&e);
    }
}

/// Sends SIGUSR1 to the other tinge processes of this user.
fn reload_others() -> usize {
    let uid = unsafe { libc::getuid() };
    let Ok(out) = Command::new("pgrep").args(["-U", &uid.to_string(), "-x", "tinge"]).output() else {
        return 0;
    };
    let me = std::process::id() as libc::pid_t;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|f| f.parse::<libc::pid_t>().ok())
        .filter(|&pid| pid != me && unsafe { libc::kill(pid, SIGUSR1) } == 0)
        .count()
}

/// Streams reads of `r` into a channel, closed at the end or on an error.
fn read_all(mut r: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (tx, rx) = bounded(0);
    thread::spawn(move || {
        let mut buf = vec![0; stream::READ_SIZE];
        loop {
            match r.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    });
    rx
}

/// Standard output, unbuffered: the stream does its own buffering.
fn stdout() -> ManuallyDrop<File> {
    ManuallyDrop::new(unsafe { File::from_raw_fd(1) })
}

/// Reports a write error. A closed pipe (`tinge | head`) ends tinge with
/// SIGPIPE, quietly, as it does other filters; `cleanup` runs first.
fn report(result: io::Result<()>, cleanup: impl FnOnce()) {
    match result {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
            cleanup();
            // Rust ignores SIGPIPE, which made the write fail instead
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                libc::raise(libc::SIGPIPE);
            }
            std::process::exit(128 + libc::SIGPIPE);
        }
        Err(e) => eprint(&e.to_string()),
        Ok(()) => {}
    }
}

fn run() -> u8 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut o = match parse_args(&args) {
        Ok(o) => o,
        Err(e) => {
            let _ = writeln!(io::stderr(), "{USAGE}tinge: error: {e}");
            return 2;
        }
    };
    if o.help {
        report(stdout().write_all(USAGE.as_bytes()), || ());
        return 0;
    }
    if o.version {
        report(writeln!(stdout(), "tinge {}", env!("CARGO_PKG_VERSION")), || ());
        return 0;
    }
    if !o.rgb {
        o.rgb = matches!(std::env::var("COLORTERM").as_deref(), Ok("truecolor" | "24bit"));
    }
    if o.reload {
        report(writeln!(stdout(), "Processes reloaded: {}", reload_others()), || ());
        return 0;
    }
    if o.config.is_empty() {
        o.config = select::select_set(&select::target_of(&o.program, &o.tags), &rules::user_dir());
    }
    if o.print {
        let Some(data) = read_rules(&o.config) else { return 1 };
        let _ = writeln!(io::stderr(), "# rule set: {}", o.config);
        report(stdout().write_all(&data), || ());
        return 0;
    }
    // The set is looked up again on every reload, so creating
    // ~/.config/tinge/<set>.yml and running tinge -r takes effect
    let cfg = Arc::new(Mutex::new(Config::new(o.benchmark)));
    load(&cfg, &o.config, o.rgb);

    // SIGUSR1 reloads the config. Ctrl-C is meant for the program, so tinge
    // keeps running to show the rest of its output: SIGINT is caught and
    // dropped, not ignored, since an ignored signal stays ignored in the
    // program after exec.
    let mut signals = Signals::new([SIGUSR1, SIGINT]).expect("signal handlers");
    {
        let (cfg, spec, rgb) = (cfg.clone(), o.config.clone(), o.rgb);
        thread::spawn(move || {
            for sig in signals.forever() {
                if sig == SIGUSR1 {
                    load(&cfg, &spec, rgb);
                }
            }
        });
    }

    let code = if o.program.is_empty() {
        report(stream::process(&*cfg, &read_all(io::stdin()), None, &*stdout()), || ());
        0
    } else {
        pty::run_program(&o.program, &*cfg)
    };

    if o.benchmark {
        print_benchmark(&cfg.lock().unwrap().benchmarks());
    }
    code
}

fn print_benchmark(results: &[&Benchmark]) {
    let total: f64 = results.iter().map(|b| b.duration.as_secs_f64()).sum();
    let mut err = io::stderr().lock();
    if !results.is_empty() {
        let _ = writeln!(err, "Benchmark results (time spent, match count):");
    }
    for b in results {
        let secs = b.duration.as_secs_f64();
        let share = if total > 0.0 { secs / total * 100.0 } else { 0.0 };
        let name = if b.rule.description.is_empty() {
            format!("{:?}", b.rule.regex.chars().take(30).collect::<String>())
        } else {
            b.rule.description.clone()
        };
        let _ = writeln!(err, "{share:6.2}% {secs:.3}s  {:<7}  {name}", b.count);
    }
}

fn main() -> ExitCode {
    ExitCode::from(run())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse() {
        let o = parse_args(&args(&["-bR", "--config=x.yml", "ssh", "-c", "aes", "host"])).unwrap();
        assert!(o.benchmark && o.rgb && !o.reload);
        assert_eq!((o.config.as_str(), o.program.join(" ")), ("x.yml", "ssh -c aes host".into()));

        let o = parse_args(&args(&["-t", "a,b c", "--tags=d", "-c", "network", "-", "x"])).unwrap();
        assert_eq!(o.tags, ["a", "b", "c", "d"]);
        assert_eq!((o.config.as_str(), o.program.as_slice()), ("network", &args(&["-", "x"])[..]));

        let o = parse_args(&args(&["-r", "--", "-b"])).unwrap();
        assert!(o.reload && !o.benchmark);
        assert_eq!(o.program, ["-b"]);

        assert!(parse_args(&args(&["--nope"])).is_err());
        assert!(parse_args(&args(&["-bx"])).is_err());
        assert_eq!(parse_args(&args(&["-c"])).unwrap_err(), "argument -c/--config: expected one argument");
    }
}
