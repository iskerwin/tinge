//! Running a program in a pseudo-terminal: input, window size and signals
//! are passed on, and its output is highlighted.

use std::env;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crossbeam_channel::bounded;
use signal_hook::consts::{SIGHUP, SIGTERM, SIGWINCH};
use signal_hook::iterator::Signals;
use tinge::stream::{self, Highlighter};

use crate::{eprint, read_all, report, stdout};

/// Where exec would find `name`, like Go's exec.LookPath: a name with a '/'
/// as is, others in $PATH; it must be an executable file.
fn look_path(name: &str) -> Option<PathBuf> {
    let executable = |p: &Path| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if name.contains('/') {
        return executable(Path::new(name)).then(|| name.into());
    }
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir }.join(name))
        .find(|p| executable(p))
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r == -1 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

fn get_termios(fd: RawFd) -> Option<libc::termios> {
    let mut t = MaybeUninit::uninit();
    (unsafe { libc::tcgetattr(fd, t.as_mut_ptr()) } == 0).then(|| unsafe { t.assume_init() })
}

fn set_termios(fd: RawFd, t: &libc::termios) {
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, t) };
}

/// Gives `to` the window size of `from`.
fn copy_size(from: RawFd, to: RawFd) {
    let mut ws = MaybeUninit::<libc::winsize>::uninit();
    if unsafe { libc::ioctl(from, libc::TIOCGWINSZ, ws.as_mut_ptr()) } == 0 {
        unsafe { libc::ioctl(to, libc::TIOCSWINSZ, ws.as_ptr()) };
    }
}

/// Opens a pseudo-terminal, both ends close-on-exec: (master, slave).
fn open_pty() -> io::Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (0, 0);
    let null = std::ptr::null_mut();
    cvt(unsafe { libc::openpty(&mut master, &mut slave, null, null as _, null as _) })?;
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
    }
    Ok((master, slave))
}

/// Restores the terminal settings when dropped.
struct Restore(Option<libc::termios>);

impl Restore {
    fn now(&self) {
        if let Some(t) = &self.0 {
            set_termios(0, t);
        }
    }
}

impl Drop for Restore {
    fn drop(&mut self) {
        self.now();
    }
}

/// Runs `args` in a pseudo-terminal, forwarding stdin to it and highlighting
/// its output; returns the program's exit status.
pub fn run_program<H: Highlighter + ?Sized>(args: &[String], h: &H) -> u8 {
    let Some(path) = look_path(&args[0]) else {
        eprint(&format!("{}: command not found", args[0]));
        return 127;
    };
    let attrs = get_termios(0);

    let (master, slave) = match open_pty() {
        Ok(p) => p,
        Err(e) => {
            eprint(&format!("cannot open a pseudo-terminal: {e}"));
            return 1;
        }
    };
    if let Some(attrs) = &attrs {
        // The program's terminal starts with the settings ours had
        set_termios(slave.as_raw_fd(), attrs);
        copy_size(0, master.as_raw_fd());
    }

    // Raw mode before starting, so nothing typed early is echoed twice; the
    // pseudo-terminal does the line processing
    let restore = Restore(attrs);
    if let Some(attrs) = &attrs {
        let mut raw = *attrs;
        unsafe { libc::cfmakeraw(&mut raw) };
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        set_termios(0, &raw);
    }

    let child = (|| {
        let mut cmd = Command::new(&path);
        cmd.arg0(&args[0]).args(&args[1..]);
        cmd.stdin(Stdio::from(slave.try_clone()?));
        cmd.stdout(Stdio::from(slave.try_clone()?));
        cmd.stderr(Stdio::from(slave.try_clone()?));
        unsafe {
            cmd.pre_exec(|| {
                cvt(libc::setsid())?;
                cvt(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
                Ok(())
            });
        }
        cmd.spawn()
        // cmd, and its copies of the slave, are dropped here
    })();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            drop(restore);
            eprint(&format!("{}: {e}", args[0]));
            return 126;
        }
    };
    let pid = child.id() as libc::pid_t;
    let master = File::from(master);

    // The slave stays open here until the program has exited and its output
    // is read: once the last slave is closed, macOS may drop what is still
    // buffered, and a quick program's output would be lost
    let (exited_tx, exited) = bounded(1);
    let drain_fd = master.as_raw_fd();
    thread::spawn(move || {
        let status = child.wait();
        drain(drain_fd);
        drop(slave); // the reader now gets EIO, ending the stream
        let _ = exited_tx.send(status);
    });

    let mut signals = Signals::new([SIGWINCH, SIGTERM, SIGHUP]).expect("signal handlers");
    let handle = signals.handle();
    let (tty, size_fd) = (attrs.is_some(), master.as_raw_fd());
    thread::spawn(move || {
        for sig in signals.forever() {
            if sig == SIGWINCH {
                if tty {
                    copy_size(0, size_fd);
                }
                continue;
            }
            // Leave the terminal usable and pass the signal on
            if let Some(t) = &attrs {
                set_termios(0, t);
            }
            unsafe { libc::kill(pid, sig) };
            std::process::exit(128 + sig);
        }
    });

    let (typed_tx, typed) = bounded(1);
    let mut input = master.try_clone().expect("dup");
    thread::spawn(move || {
        let mut buf = vec![0; stream::READ_SIZE];
        let mut stdin = io::stdin().lock();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    if input.write_all(&buf[..n]).is_err() {
                        return;
                    }
                    let _ = typed_tx.try_send(());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    });

    let output = master.try_clone().expect("dup");
    // Ending on a closed pipe closes the master too, which hangs up the
    // program
    report(stream::process(h, &read_all(output), Some(&typed), &*stdout()), || restore.now());
    let status = exited.recv().expect("wait");
    handle.close();
    drop(master);
    drop(restore);
    match status {
        Ok(s) => match (s.code(), s.signal()) {
            (Some(code), _) => code as u8,
            (None, Some(sig)) => (128 + sig) as u8,
            _ => 1,
        },
        Err(e) => {
            eprint(&e.to_string());
            1
        }
    }
}

/// Waits until the program's output has been read: no data pending on the
/// master for a short while.
fn drain(fd: RawFd) {
    let mut fds = [libc::pollfd { fd, events: libc::POLLIN, revents: 0 }];
    for _ in 0..500 {
        // at most ~10 s
        match unsafe { libc::poll(fds.as_mut_ptr(), 1, 20) } {
            0 => return,
            -1 if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted => return,
            _ => {}
        }
        fds[0].revents = 0;
        thread::sleep(Duration::from_millis(1)); // let the reader take it
    }
}
