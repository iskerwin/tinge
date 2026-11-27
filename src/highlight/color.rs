//! Rule colors ("f.red b#123456 bold") and the SGR sequences already in the
//! output.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use regex::bytes::Regex as BytesRegex;

/// The attribute an SGR code sets. A full reset (SGR 0), or an SGR that is
/// not a color and is left untouched, has no type (`None` where a type is
/// optional).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Type {
    Fg,
    Bg,
    Blink,
    Bold,
    Invert,
    Italic,
    Strike,
    Underline,
}

pub const NUM_TYPES: usize = 8;

impl Type {
    pub const ALL: [Type; NUM_TYPES] =
        [Type::Fg, Type::Bg, Type::Blink, Type::Bold, Type::Invert, Type::Italic, Type::Strike, Type::Underline];
    const STYLES: [Type; 6] = [Type::Blink, Type::Bold, Type::Invert, Type::Italic, Type::Strike, Type::Underline];

    pub fn index(self) -> usize {
        self as usize
    }

    fn name(self) -> &'static str {
        ["fg", "bg", "blink", "bold", "invert", "italic", "strike", "underline"][self.index()]
    }

    /// The SGR that turns the attribute off.
    pub fn reset(self) -> &'static [u8] {
        [b"\x1b[39m", b"\x1b[49m", b"\x1b[25m", b"\x1b[22m", b"\x1b[27m", b"\x1b[23m", b"\x1b[29m", b"\x1b[24m"]
            [self.index()]
    }

    /// The SGR of a style; fg and bg have none.
    fn code(self) -> &'static [u8] {
        [b"" as &[u8], b"", b"\x1b[5m", b"\x1b[1m", b"\x1b[7m", b"\x1b[3m", b"\x1b[9m", b"\x1b[4m"][self.index()]
    }

    /// The type a single SGR parameter (not 38 / 48) sets.
    fn of_param(n: u64) -> Option<Type> {
        Some(match n {
            30..=37 | 39 | 90..=97 => Type::Fg,
            40..=47 | 49 | 100..=107 => Type::Bg,
            5 | 25 => Type::Blink,
            1 | 2 | 22 => Type::Bold,
            7 | 27 => Type::Invert,
            3 | 23 => Type::Italic,
            9 | 29 => Type::Strike,
            4 | 24 => Type::Underline,
            _ => return None,
        })
    }
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap()
}

static PALETTE_COLOR: LazyLock<Regex> = LazyLock::new(|| re(r"(?-u:\b)([bf])\.([a-z0-9_-]+)(?-u:\b)"));
static HEX_COLOR: LazyLock<Regex> = LazyLock::new(|| re(r"([bf])#([0-9a-f]{6})"));
static STYLE: LazyLock<Regex> = LazyLock::new(|| re(r"blink|bold|invert|italic|strike|underline"));
static COLOR_FORMAT: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(?:(?:[bf]#[0-9a-f]{6}|blink|bold|invert|italic|strike|underline)(?:[\t\n\f\r ]+|$))+$"));
static PALETTE_NAME: LazyLock<Regex> = LazyLock::new(|| re(r"^[a-z0-9_-]+$"));
static PALETTE_VALUE: LazyLock<Regex> = LazyLock::new(|| re(r"^#[0-9a-f]{6}$"));
static SGR: LazyLock<BytesRegex> =
    LazyLock::new(|| BytesRegex::new(r"(?-u)\x1b\x5b[\x30-\x3f]*[\x20-\x2f]*\x6d").unwrap());
static SGR_COLOR: LazyLock<BytesRegex> = LazyLock::new(|| BytesRegex::new(r"(?-u)\x1b\x5b[0-9;]*\x6d").unwrap());

/// One attribute of a color: its type and the SGR that sets it.
#[derive(Clone, Debug, PartialEq)]
pub struct Code {
    pub typ: Type,
    pub sgr: Vec<u8>,
}

/// A parsed rule color.
#[derive(Clone, Debug, PartialEq)]
pub struct Color {
    pub codes: Vec<Code>,
}

/// Lower-case names to "#rrggbb".
pub type Palette = HashMap<String, String>;

/// Validates and adds a palette color.
pub fn add_color(palette: &mut Palette, name: &str, value: &str) -> Result<(), String> {
    let name = name.trim().to_lowercase();
    let value = value.trim().to_lowercase();
    if Type::ALL.iter().any(|t| t.name() == name) {
        return Err("color name is reserved".into());
    }
    if palette.contains_key(&name) {
        return Err("a color with the same name already exists".into());
    }
    if !PALETTE_NAME.is_match(&name) {
        return Err("name accepts alphanumerics, dashes, and underscores only".into());
    }
    if !PALETTE_VALUE.is_match(&value) {
        return Err("palette color must be in `#123abc` format".into());
    }
    palette.insert(name, value);
    Ok(())
}

fn resolve(palette: &Palette, color: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut last = 0;
    for c in PALETTE_COLOR.captures_iter(color) {
        let name = &c[2];
        let value = palette.get(name).ok_or_else(|| format!("color {} not in palette", py_repr(name)))?;
        let whole = c.get(0).unwrap();
        out += &color[last..whole.start()];
        out += &c[1];
        out += value;
        last = whole.end();
    }
    out += &color[last..];
    Ok(out)
}

/// Parses a color string; `rgb` selects 24-bit codes over xterm-256.
pub fn parse_color(spec: &str, palette: Option<&Palette>, rgb: bool) -> Result<Color, String> {
    let mut value = spec.trim().to_lowercase();
    if PALETTE_COLOR.is_match(&value) {
        let palette = palette.ok_or("palette color name present, but no palette specified")?;
        value = resolve(palette, &value)?;
    }
    if !COLOR_FORMAT.is_match(&value) {
        return Err(format!("invalid color format {}", py_repr(&value)));
    }

    let mut codes = Vec::new();
    let (mut fg, mut bg) = (false, false);
    for c in HEX_COLOR.captures_iter(&value) {
        let (typ, prefix, seen) = if &c[1] == "b" { (Type::Bg, "48", &mut bg) } else { (Type::Fg, "38", &mut fg) };
        if *seen {
            return Err("color accepts one foreground and one background colors".into());
        }
        *seen = true;
        let hex = |i: usize| u8::from_str_radix(&c[2][i..i + 2], 16).unwrap();
        let (r, g, b) = (hex(0), hex(2), hex(4));
        let id = if rgb { format!("2;{r};{g};{b}") } else { format!("5;{}", rgb_to_xterm256(r, g, b)) };
        codes.push(Code { typ, sgr: format!("\x1b[{prefix};{id}m").into_bytes() });
    }
    let mut styles: Vec<&str> = Vec::new();
    for m in STYLE.find_iter(&value) {
        if !styles.contains(&m.as_str()) {
            styles.push(m.as_str());
        }
    }
    for s in styles {
        let t = Type::STYLES.into_iter().find(|t| t.name() == s).unwrap();
        codes.push(Code { typ: t, sgr: t.code().to_vec() });
    }
    Ok(Color { codes })
}

/// Downscales 24-bit RGB to the closest xterm-256 color.
pub fn rgb_to_xterm256(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    fn index(v: i32, steps: &[i32]) -> usize {
        let mut best = 0;
        for (i, s) in steps.iter().enumerate() {
            if (s - v).abs() < (steps[best] - v).abs() {
                best = i;
            }
        }
        best
    }
    let distance = |nr: i32, ng: i32, nb: i32| (nr - r).pow(2) + (ng - g).pow(2) + (nb - b).pow(2);
    const RGB: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let (ri, gi, bi) = (index(r, &RGB), index(g, &RGB), index(b, &RGB));
    let rgb_distance = distance(RGB[ri], RGB[gi], RGB[bi]);

    let gray: Vec<i32> = (8..239).step_by(10).collect();
    let gray_index = index((r + g + b) / 3, &gray);
    let gv = gray[gray_index];
    if distance(gv, gv, gv) < rgb_distance {
        return 232 + gray_index as u8;
    }
    (16 + 36 * ri + 6 * gi + bi) as u8
}

/// A color code to be placed at `pos` (an index into stripped data).
#[derive(Clone, Debug, PartialEq)]
pub struct Insert {
    pub pos: usize,
    pub code: Vec<u8>,
    pub is_reset: bool,
    pub typ: Option<Type>,
}

fn sgr(code: &str) -> Vec<u8> {
    format!("\x1b[{code}m").into_bytes()
}

/// Splits an SGR sequence into its individual colors.
fn decode_sgr(src: &[u8], is_reset: bool) -> Vec<Insert> {
    let insert = |code, is_reset, typ| Insert { pos: 0, code, is_reset, typ };
    let untouched = vec![insert(src.to_vec(), false, None)];
    if !SGR_COLOR.is_match(src) {
        return untouched;
    }
    let start = src.iter().position(|&b| b != 0x1b && b != b'[').unwrap_or(src.len());
    let end = src.iter().rposition(|&b| b != b'm').map_or(start, |i| i + 1).max(start);
    // Only digits and ';' are left
    let body = std::str::from_utf8(&src[start..end]).unwrap();
    let codes: Vec<&str> = body.split(';').collect();
    let mut colors = Vec::new();
    let mut i = 0;
    while i < codes.len() {
        let code = codes[i];
        let n: u64 = if code.is_empty() { 0 } else { code.parse().unwrap_or(u64::MAX) };
        if n == 0 {
            colors.push(insert(sgr("0"), true, None));
        } else if code == "38" || code == "48" {
            let typ = if code == "38" { Type::Fg } else { Type::Bg };
            let joined = if codes.len() > i + 2 && codes[i + 1] == "5" {
                i += 2;
                codes[i - 2..=i].join(";")
            } else if codes.len() > i + 4 && codes[i + 1] == "2" {
                i += 4;
                codes[i - 4..=i].join(";")
            } else {
                return untouched;
            };
            colors.push(insert(sgr(&joined), is_reset, Some(typ)));
        } else {
            let code = sgr(&n.to_string());
            match Type::of_param(n) {
                Some(t) => {
                    let reset = is_reset || code == t.reset();
                    colors.push(insert(code, reset, Some(t)));
                }
                None => colors.push(insert(code, false, None)),
            }
        }
        i += 1;
    }
    colors
}

/// Removes the SGR sequences from data, returning them as inserts in
/// descending position order (the order the highlighter keeps inserts in).
/// It searches again from the start after each removal, since removing one
/// sequence can join the bytes around it into another.
pub fn strip_colors(data: &[u8]) -> (Vec<u8>, Vec<Insert>) {
    let mut data = data.to_vec();
    let mut inserts = Vec::new();
    while let Some(m) = SGR.find(&data) {
        let (start, end) = (m.start(), m.end());
        for mut c in decode_sgr(&data[start..end], true) {
            c.pos = start;
            inserts.push(c);
        }
        data.drain(start..end);
    }
    inserts.reverse();
    (data, inserts)
}

/// Quotes a string for error messages like Python's repr: 'x', or "x" when it
/// has a quote.
pub fn py_repr(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') { format!("\"{s}\"") } else { format!("'{}'", s.replace('\'', "\\'")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xterm256() {
        for (r, g, b, want) in
            [(0, 0, 0, 16), (255, 255, 255, 231), (128, 128, 128, 244), (0x50, 0xfa, 0x7b, 84), (0xff, 0x55, 0x55, 203)]
        {
            assert_eq!(rgb_to_xterm256(r, g, b), want, "{r} {g} {b}");
        }
    }

    #[test]
    fn colors() {
        let mut p = Palette::new();
        add_color(&mut p, "Green", " #50FA7B").unwrap();
        assert!(add_color(&mut p, "bold", "#000000").is_err());
        assert!(add_color(&mut p, "green", "#000000").is_err());
        let c = parse_color("f.green bold b#000000 bold", Some(&p), false).unwrap();
        let sgrs: Vec<&[u8]> = c.codes.iter().map(|c| c.sgr.as_slice()).collect();
        assert_eq!(sgrs, [b"\x1b[38;5;84m" as &[u8], b"\x1b[48;5;16m", b"\x1b[1m"]);
        assert_eq!(
            parse_color("f.green", None, false).unwrap_err(),
            "palette color name present, but no palette specified"
        );
        assert_eq!(parse_color("f.blue", Some(&p), false).unwrap_err(), "color 'blue' not in palette");
        assert!(parse_color("f#111111 f#222222", None, true).is_err());
        assert!(parse_color("bolder", None, true).is_err());
    }

    #[test]
    fn strip() {
        let (data, inserts) = strip_colors(b"a\x1b[1;31mb\x1b[0mc\x1b[38;5;1md");
        assert_eq!(data, b"abcd");
        let got: Vec<(usize, &[u8], bool, Option<Type>)> =
            inserts.iter().map(|i| (i.pos, i.code.as_slice(), i.is_reset, i.typ)).collect();
        assert_eq!(
            got,
            [
                (3, b"\x1b[38;5;1m" as &[u8], true, Some(Type::Fg)),
                (2, b"\x1b[0m", true, None),
                (1, b"\x1b[31m", true, Some(Type::Fg)),
                (1, b"\x1b[1m", true, Some(Type::Bold)),
            ]
        );
    }
}
