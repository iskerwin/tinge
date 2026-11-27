//! The binary end to end: in a pseudo-terminal like a user's shell, and in a
//! pipe.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_tinge");

const TEST_RULES: &str = "palette:\n  green: \"#50FA7B\"\nrules:\n  - regex: '\\bup\\b'\n    color: f.green\n";

/// A fresh directory, used as HOME so the user's rules are never read.
fn tempdir() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir =
        std::env::temp_dir().join(format!("tinge-cli-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_rules() -> String {
    let path = tempdir().join("rules.yml");
    fs::write(&path, TEST_RULES).unwrap();
    path.to_str().unwrap().to_owned()
}

fn command(args: &[&str]) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.args(args).env("HOME", tempdir()).env("XDG_CONFIG_HOME", "").env("COLORTERM", "");
    cmd
}

/// What the user does once the program has printed `ready`.
enum Then<'a> {
    Type(&'a str),
    Resize(u16, u16),
}

fn winsize(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

/// Runs tinge in a pseudo-terminal of the given size and returns everything
/// printed and the exit status (negative: the signal that killed it). The
/// input, when given, happens once the output contains `ready`, which the
/// program prints when tinge is set up and the program is running.
fn in_pty(rows: u16, cols: u16, then: Option<Then>, args: &[&str]) -> (String, i32) {
    let (mut master, mut slave) = (0, 0);
    let ws = winsize(rows, cols);
    let null = std::ptr::null_mut();
    assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, null, null as _, &ws as *const _ as _) }, 0);
    for fd in [master, slave] {
        // Tests run in parallel: other tests' children must not hold these
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    let (master, slave) = unsafe { (File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut cmd = command(args);
    cmd.stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    drop(cmd);
    drop(slave);

    let out = Arc::new(Mutex::new(Vec::new()));
    let mut r = master.try_clone().unwrap();
    let reader = {
        let out = out.clone();
        thread::spawn(move || {
            let mut buf = [0; 4096];
            // Ends with EIO once tinge exits
            while let Ok(n) = r.read(&mut buf) {
                if n == 0 {
                    break;
                }
                out.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        })
    };
    let text = || String::from_utf8_lossy(&out.lock().unwrap()).into_owned();
    if let Some(then) = then {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !text().contains("ready") {
            assert!(Instant::now() < deadline && !reader.is_finished(), "no ready in {:?}", text());
            thread::sleep(Duration::from_millis(5));
        }
        match then {
            Then::Type(input) => (&master).write_all(input.as_bytes()).unwrap(),
            // The terminal sends tinge SIGWINCH
            Then::Resize(rows, cols) => {
                assert_eq!(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &winsize(rows, cols)) }, 0);
            }
        }
    }
    let status = child.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !reader.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    (text(), status.code().unwrap_or_else(|| -status.signal().unwrap()))
}

#[test]
fn program_output_is_highlighted() {
    let (out, code) = in_pty(24, 80, None, &["-c", &write_rules(), "printf", r"link up\n"]);
    assert_eq!(code, 0, "{out:?}");
    assert!(out.contains("link \x1b[38;5;84mup\x1b[39m\r\n"), "{out:?}");
}

#[test]
fn exit_status() {
    assert_eq!(in_pty(24, 80, None, &["-c", &write_rules(), "sh", "-c", "exit 3"]).1, 3);
    assert_eq!(in_pty(24, 80, None, &["-c", &write_rules(), "tinge-no-such-command"]).1, 127);
}

#[test]
fn ctrl_c_reaches_the_program() {
    // The program must not inherit an ignored SIGINT; killed by it, its
    // status is 128 + 2
    let program = "echo ready; exec sleep 5";
    let (out, code) = in_pty(24, 80, Some(Then::Type("\x03")), &["-c", &write_rules(), "sh", "-c", program]);
    assert_eq!(code, 130, "{out:?}");
}

#[test]
fn window_size_is_passed_on() {
    let (out, _) = in_pty(30, 100, None, &["-c", &write_rules(), "stty", "size"]);
    assert!(out.contains("30 100"), "{out:?}");
}

#[test]
fn window_resizes_are_passed_on() {
    let program = r#"trap 'stty size; exit' WINCH; echo ready; for i in $(seq 200); do sleep 0.05; done"#;
    let (out, code) = in_pty(24, 80, Some(Then::Resize(40, 120)), &["-c", &write_rules(), "sh", "-c", program]);
    assert_eq!(code, 0, "{out:?}");
    assert!(out.contains("40 120"), "{out:?}");
}

#[test]
fn input_is_forwarded() {
    let program = r#"echo ready; read x; echo "got $x""#;
    let (out, code) = in_pty(24, 80, Some(Then::Type("up\r")), &["-c", &write_rules(), "sh", "-c", program]);
    assert_eq!(code, 0, "{out:?}");
    assert!(out.contains("got \x1b[38;5;84mup\x1b[39m"), "{out:?}");
}

fn piped(stdin: &[u8], args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = command(args);
    cmd.envs(env.iter().copied()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut w = child.stdin.take().unwrap();
    let stdin = stdin.to_vec();
    thread::spawn(move || {
        let _ = w.write_all(&stdin);
    });
    child.wait_with_output().unwrap()
}

#[test]
fn pipe_mode() {
    let out = piped(b"is up\nnot upper\n", &["-c", &write_rules()], &[("COLORTERM", "truecolor")]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "is \x1b[38;2;80;250;123mup\x1b[39m\nnot upper\n");
}

#[test]
fn closed_pipe_ends_tinge_like_other_filters() {
    // yes | tinge | head -1: quietly killed by SIGPIPE, as cat or grep are
    let mut cmd = command(&["-c", "server"]);
    let mut yes = Command::new("yes").arg("up").stdout(Stdio::piped()).spawn().unwrap();
    cmd.stdin(yes.stdout.take().unwrap()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    drop(cmd); // its copy of the pipe would keep yes running
    let mut first = [0; 3];
    child.stdout.take().unwrap().read_exact(&mut first).unwrap(); // then closed
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.signal(), Some(libc::SIGPIPE), "{:?}", out.status);
    assert!(out.stderr.is_empty(), "{:?}", String::from_utf8_lossy(&out.stderr));
    yes.wait().unwrap(); // ends with SIGPIPE once tinge has exited
}

#[test]
fn built_in_rules() {
    let run = |stdin: &str, args: &[&str]| {
        let out = piped(stdin.as_bytes(), args, &[]);
        let text = String::from_utf8_lossy(&[out.stdout, out.stderr].concat()).into_owned();
        assert!(out.status.success() && !text.contains("tinge:"), "{args:?}: {text:?}");
        text
    };
    assert!(run("GigabitEthernet0/1 is up\n", &["-c", "network"]).contains("\x1b[1mGigabitEthernet0/1"));
    assert!(run("Active: active (running)\n", &["-c", "server"]).contains("\x1b[1mactive (running)"));
    assert!(run("", &["-c", "network", "--print-rules"]).contains("Interface name"));
    assert!(run("", &["-c", "server", "--print-rules"]).contains("systemd"));
}

#[test]
fn print_rules_names_the_set() {
    let out = piped(b"", &["--tags", "router", "--print-rules", "ssh", "gw"], &[]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stderr), "# rule set: network\n");
    assert!(String::from_utf8_lossy(&out.stdout).contains("Interface name"));
}

#[test]
fn usage_and_version() {
    let out = piped(b"", &["--nope"], &[]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).ends_with("tinge: error: unrecognized arguments: --nope\n"));
    let out = piped(b"", &["-v"], &[]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("tinge {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn user_set_is_found_on_reload() {
    // SIGUSR1 reads the set again: a ~/.config/tinge/<set>.yml created later
    // takes effect
    let home = tempdir();
    let mut child = Command::new(BIN)
        .args(["-c", "lab"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", "")
        .env("COLORTERM", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The set is missing when tinge starts
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    err.read_line(&mut line).unwrap();
    assert!(line.contains("no rule set \"lab\""), "{line:?}");

    let dir = home.join(".config/tinge");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("lab.yml"), TEST_RULES).unwrap();
    unsafe { libc::kill(child.id() as i32, libc::SIGUSR1) };
    // Lines are plain until the reload has happened
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        stdin.write_all(b"is up\n").unwrap();
        line.clear();
        out.read_line(&mut line).unwrap();
        if line != "is up\n" {
            break;
        }
        assert!(Instant::now() < deadline, "not reloaded");
    }
    assert_eq!(line, "is \x1b[38;5;84mup\x1b[39m\n");
    drop(stdin);
    assert!(child.wait().unwrap().success());
}
