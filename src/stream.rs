//! Buffers terminal output into chunks that can be highlighted
//! independently, and decides when to wait for more.

use std::io::{self, BufWriter, Write};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, never, select};
use regex::bytes::Regex;

/// The maximum chunk read at once.
pub const READ_SIZE: usize = 8192;

/// The sequences the output is split on (ECMA-035/048): new lines and
/// vertical spaces, private and independent control functions, character set
/// selection, CSI other than SGR, and control strings.
static SPLIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?s-u)(\r\n?|[\n\x0b\x0c]|",
        r"\x1b[\x30-\x4f\x51-\x57\x59-\x5a\x5c\x60-\x7e]|",
        r"\x1b[\x23\x28-\x2b\x2d-\x2f][\x20-\x7e]|",
        r"\x1b\x5b[\x30-\x3f]*[\x20-\x2f]*[\x40-\x6c\x6e-\x7e]|",
        r"\x1b[\x50\x58\x5d\x5e\x5f][^\x07\x1b]*(?:\x07|\x1b\x5c)?)",
    ))
    .unwrap()
});

const CONTROL_STRING_START: [&[u8]; 5] = [b"\x1bP", b"\x1bX", b"\x1b]", b"\x1b^", b"\x1b_"];
const CONTROL_STRING_END: [&[u8]; 2] = [b"\x07", b"\x1b\\"];

/// Data to highlight followed by a separator written unchanged.
#[derive(Debug, PartialEq)]
pub struct Chunk<'a> {
    pub data: &'a [u8],
    pub sep: &'a [u8],
}

/// Cuts buf into chunks; the last chunk may have an empty separator.
pub fn split(buf: &[u8]) -> Vec<Chunk<'_>> {
    let mut chunks = Vec::new();
    let mut last = 0;
    for m in SPLIT.find_iter(buf) {
        chunks.push(Chunk { data: &buf[last..m.start()], sep: m.as_bytes() });
        last = m.end();
    }
    if last < buf.len() || chunks.is_empty() {
        chunks.push(Chunk { data: &buf[last..], sep: b"" });
    }
    chunks
}

/// Colors chunks of data; `highlight_all` must equal calling `highlight` on
/// each chunk in order.
pub trait Highlighter {
    fn highlight(&self, data: &[u8]) -> Vec<u8>;
    fn highlight_all(&self, chunks: &[&[u8]]) -> Vec<Vec<u8>>;
}

/// Reads data until it ends, highlighting chunks with `h` and writing them to
/// `out`. `typed`, when given, receives a value whenever the user's input was
/// forwarded: typing cancels the wait for more output.
pub fn process<H: Highlighter + ?Sized, W: Write>(
    h: &H,
    data: &Receiver<Vec<u8>>,
    typed: Option<&Receiver<()>>,
    out: W,
) -> io::Result<()> {
    let mut w = BufWriter::with_capacity(4 * READ_SIZE, out);
    let mut buffer: Vec<u8> = Vec::new();
    let mut pending: Option<Vec<u8>> = None;
    let min_wait = Duration::from_secs(1) / if typed.is_some() { 64 } else { 256 };

    loop {
        let (read, open) = match pending.take() {
            Some(p) => (p, true),
            None => match data.recv() {
                Ok(b) => (b, true),
                Err(_) => (Vec::new(), false),
            },
        };
        buffer.extend_from_slice(&read);
        if buffer.is_empty() && !open {
            return w.flush();
        }
        let chunks = split(&buffer);
        let (last, done) = chunks.split_last().unwrap();
        if !done.is_empty() {
            let data: Vec<&[u8]> = done.iter().map(|c| c.data).collect();
            for (out, c) in h.highlight_all(&data).iter().zip(done) {
                w.write_all(out)?;
                w.write_all(c.sep)?;
            }
        }
        w.flush()?;

        // New lines indicate long output; relax the wait duration
        let wait = if buffer.contains(&b'\n') { Duration::from_secs(1) / 8 } else { min_wait };
        let got_data = open && !read.is_empty();
        let backspaces = read.iter().filter(|&&b| b == b'\x08').count();
        let keep = |last: &Chunk| [last.data, last.sep].concat();

        let next = if got_data
            && CONTROL_STRING_START.iter().any(|p| last.sep.starts_with(p))
            && !CONTROL_STRING_END.iter().any(|s| last.sep.ends_with(s))
        {
            // The separator is an incomplete control string; wait for the rest
            keep(last)
        } else if !buffer.is_empty() && buffer.len() < 2 + backspaces * 2 {
            // A single character is keyboard echo; don't highlight it (zsh
            // adds backspaces around it)
            w.write_all(last.data)?;
            w.write_all(last.sep)?;
            Vec::new()
        } else if got_data && buffer.len() <= READ_SIZE && {
            let (more, next) = more_data(data, typed, wait);
            pending = next;
            more
        } {
            // More data is coming; fetch it before highlighting the last chunk
            keep(last)
        } else {
            w.write_all(&h.highlight(last.data))?;
            w.write_all(last.sep)?;
            Vec::new()
        };
        buffer = next;
        w.flush()?;
        if !open && buffer.is_empty() {
            return Ok(());
        }
    }
}

/// Waits up to `d` for the next read, which it returns. Typing ends the wait
/// early; so does the end of data, which the next round reads again.
fn more_data(data: &Receiver<Vec<u8>>, typed: Option<&Receiver<()>>, d: Duration) -> (bool, Option<Vec<u8>>) {
    // Input forwarded before the wait started does not cancel it
    if let Some(t) = typed {
        while t.try_recv().is_ok() {}
    }
    let none = never();
    let mut typed = typed.unwrap_or(&none);
    let deadline = Instant::now() + d;
    loop {
        select! {
            recv(data) -> b => return (true, b.ok()),
            recv(typed) -> t => match t {
                Ok(()) => return (false, None),
                // Input ended: nothing more can be typed
                Err(_) => typed = &none,
            },
            default(deadline.saturating_duration_since(Instant::now())) => return (false, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{bounded, unbounded};
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// Wraps every highlighted chunk in brackets.
    struct Marker;

    impl Highlighter for Marker {
        fn highlight(&self, b: &[u8]) -> Vec<u8> {
            [b"[", b, b"]"].concat()
        }
        fn highlight_all(&self, chunks: &[&[u8]]) -> Vec<Vec<u8>> {
            chunks.iter().map(|c| self.highlight(c)).collect()
        }
    }

    fn quote(b: &[u8]) -> String {
        format!("{:?}", String::from_utf8_lossy(b))
    }

    #[test]
    fn split_on_separators() {
        for (input, want) in [
            ("a\nb", r#""a"+"\n" "b"+"""#),
            ("a\r\nb\r", r#""a"+"\r\n" "b"+"\r""#),
            ("x\x1b[2Ky", r#""x"+"\u{1b}[2K" "y"+"""#),
            ("x\x1b[31my", r#""x\u{1b}[31my"+"""#), // SGR is not a separator
            ("t\x1b]0;title\x07u", r#""t"+"\u{1b}]0;title\u{7}" "u"+"""#),
            ("t\x1b]0;unterminated", r#""t"+"\u{1b}]0;unterminated""#),
            ("a\x0bb\x0cc", r#""a"+"\u{b}" "b"+"\u{c}" "c"+"""#),
            ("\x1b(Bz", r#"""+"\u{1b}(B" "z"+"""#),
        ] {
            let got: Vec<String> =
                split(input.as_bytes()).iter().map(|c| format!("{}+{}", quote(c.data), quote(c.sep))).collect();
            assert_eq!(got.join(" "), want, "{input:?}");
        }
        // Control strings are cut on raw bytes, not UTF-8
        let chunks = split(b"t\x1b]0;\xff\xfe\x07u");
        assert_eq!(chunks[0].sep, b"\x1b]0;\xff\xfe\x07");
    }

    fn run(
        typed: Option<&Receiver<()>>,
        feed: impl FnOnce(crossbeam_channel::Sender<Vec<u8>>) + Send + 'static,
    ) -> String {
        let (tx, rx) = bounded(0);
        let feeder = thread::spawn(move || feed(tx));
        let mut out = Vec::new();
        process(&Marker, &rx, typed, &mut out).unwrap();
        feeder.join().unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn highlights_lines() {
        let got = run(None, |d| d.send(b"one\ntwo\nthree".to_vec()).unwrap());
        assert_eq!(got, "[one]\n[two]\n[three]");
    }

    #[test]
    fn joins_split_reads() {
        let got = run(None, |d| {
            d.send(b"GigabitEth".to_vec()).unwrap();
            d.send(b"ernet0/1 up\n".to_vec()).unwrap();
        });
        assert_eq!(got, "[GigabitEthernet0/1 up]\n");
    }

    #[test]
    fn leaves_echo_alone() {
        let got = run(None, |d| {
            d.send(b"a".to_vec()).unwrap();
            thread::sleep(Duration::from_millis(50));
            d.send(b"\x08b".to_vec()).unwrap();
        });
        assert_eq!(got, "a\x08b");
    }

    #[test]
    fn waits_for_control_string() {
        let got = run(None, |d| {
            d.send(b"x\x1b]0;tit".to_vec()).unwrap();
            thread::sleep(Duration::from_millis(200)); // longer than any wait
            d.send(b"le\x07yz".to_vec()).unwrap();
        });
        assert_eq!(got, "[x]\x1b]0;title\x07[yz]");
    }

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn string(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[test]
    fn typing_ends_the_wait() {
        let (typed_tx, typed_rx) = bounded(1);
        let (tx, rx) = bounded(0);
        let out = Shared::default();
        let watched = out.clone();
        let start = Instant::now();
        let feeder = thread::spawn(move || {
            // A new line relaxes the wait to 1/8 s; typing must end it sooner
            tx.send(b"line\nprompt> ".to_vec()).unwrap();
            thread::sleep(Duration::from_millis(5));
            typed_tx.send(()).unwrap();
            while !watched.string().contains("prompt") {
                thread::sleep(Duration::from_millis(1));
            }
            start.elapsed()
        });
        process(&Marker, &rx, Some(&typed_rx), out.clone()).unwrap();
        let flushed = feeder.join().unwrap();
        assert_eq!(out.string(), "[line]\n[prompt> ]");
        assert!(flushed < Duration::from_millis(80), "flushed after {flushed:?}");
    }

    #[test]
    fn ended_input_does_not_end_the_wait() {
        let (typed_tx, typed_rx) = unbounded::<()>();
        drop(typed_tx);
        let got = run(Some(&typed_rx), |d| {
            d.send(b"GigabitEth".to_vec()).unwrap();
            thread::sleep(Duration::from_millis(5));
            d.send(b"ernet0/1 up".to_vec()).unwrap();
        });
        assert_eq!(got, "[GigabitEthernet0/1 up]");
    }
}
