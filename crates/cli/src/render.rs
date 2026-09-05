use std::fmt::Write as _;
use std::io::{IsTerminal, Write};
use std::sync::{Arc, OnceLock};

use agent::Event;
use brain::count::{in_out, short};
use anyhow::{Result, bail};
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
const RESET: &str = "\x1b[0m";

/// Where an escape sequence ends.
///
/// The three scanners over painted text — `visible` and `clip` here, `fit` in
/// `screen` — differ in what they do with the characters (count their columns,
/// copy them, drop them) but not in where the sequence stops. This decides
/// that for all three. Each used to decide it alone, and they disagreed.
pub struct Escape {
    /// What shape it is, once the first character has said. `None` until then.
    kind: Option<Kind>,
    /// Inside a control string: the last character was `\x1b`, so a `\` now
    /// closes it.
    st: bool,
}

enum Kind {
    /// `ESC [`, `ESC O` — parameters and intermediates, then a byte in
    /// 0x40-0x7e.
    Control,
    /// `ESC P`, `ESC X`, `ESC ]`, `ESC ^`, `ESC _` — DCS, SOS, OSC, PM, APC.
    /// Arbitrary text closed by BEL or by ST (`ESC \`), not by any byte
    /// range: an OSC setting the window title carries a `;` and the title,
    /// and a scanner reading it as a control sequence stops on the first
    /// letter and draws the rest of the title.
    Str,
    /// A bare `ESC` with an intermediate byte, ending on a byte in 0x30-0x7e
    /// — wider than a control sequence's, which is where `ESC 7` lives.
    Bare,
    /// Over already: the first character was itself the final byte. `ESC 7`
    /// saves the cursor and `ESC 8` restores it — what `less`, `vim` and
    /// every progress bar emit most — and 0x37 is outside a control
    /// sequence's range, so reading one as a control sequence leaves it
    /// looking unfinished and eats the character after it.
    Done,
}

impl Escape {
    /// The state directly after an `\x1b`. Feed it every character that
    /// follows; it answers true on the one that closes the sequence.
    pub fn new() -> Self {
        Self { kind: None, st: false }
    }

    pub fn closed(&mut self, c: char) -> bool {
        let Some(kind) = &self.kind else {
            // The first character decides the shape, and for a two-byte
            // sequence it is also the last. `[` and `O` are final bytes by
            // the range test too, so they are matched before it.
            let kind = match c {
                '[' | 'O' => Kind::Control,
                'P' | 'X' | ']' | '^' | '_' => Kind::Str,
                c if ('\x30'..='\x7e').contains(&c) => Kind::Done,
                _ => Kind::Bare,
            };
            let done = matches!(kind, Kind::Done);
            self.kind = Some(kind);
            return done;
        };
        match kind {
            Kind::Done => true,
            Kind::Control => ('\x40'..='\x7e').contains(&c),
            Kind::Bare => ('\x30'..='\x7e').contains(&c),
            Kind::Str => {
                let closed = c == '\x07' || (self.st && c == '\\');
                self.st = c == '\x1b';
                closed
            }
        }
    }
}

impl Default for Escape {
    fn default() -> Self {
        Self::new()
    }
}

/// The characters a terminal actually shows, escapes stepped over: they cost
/// a dozen bytes and zero columns, so anything measuring or reproducing what
/// is on screen has to skip them the same way.
fn visible(s: &str) -> impl Iterator<Item = char> + '_ {
    let mut chars = s.chars();
    std::iter::from_fn(move || {
        loop {
            let c = chars.next()?;
            if c != '\x1b' {
                return Some(c);
            }
            let mut esc = Escape::new();
            for c in chars.by_ref() {
                if esc.closed(c) {
                    break;
                }
            }
        }
    })
}

/// The columns a painted string occupies, which is what a layout has to
/// budget for — not its byte or character count.
pub fn visible_width(s: &str) -> usize {
    visible(s)
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// The visible text of a painted string, for tests that assert on layout
/// rather than colour.
#[cfg(test)]
pub fn strip_ansi(s: &str) -> String {
    visible(s).collect()
}

/// One text attribute: bold, dim, italic — whatever SGR can set besides colour.
///
/// `Other` passes a custom parameter list through unchanged ("8" hidden, "21"
/// double underline), for anything the fixed set does not name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attr {
    Bold,
    Dim,
    Italic,
    Underline,
    Blink,
    Reverse,
    Strike,
    Other(String),
}

// Name in config, variant, SGR code — one row per named attribute, so adding
// one touches a single place instead of two parallel matches.
const NAMED_ATTRS: &[(&str, Attr, &str)] = &[
    ("bold", Attr::Bold, "1"),
    ("dim", Attr::Dim, "2"),
    ("italic", Attr::Italic, "3"),
    ("underline", Attr::Underline, "4"),
    ("blink", Attr::Blink, "5"),
    ("reverse", Attr::Reverse, "7"),
    ("strike", Attr::Strike, "9"),
];

impl Attr {
    /// A known name, or else any non-empty `;`-separated SGR parameter list.
    fn parse(s: &str) -> Result<Self> {
        if let Some((_, attr, _)) = NAMED_ATTRS.iter().find(|(name, _, _)| *name == s) {
            return Ok(attr.clone());
        }
        let ok = !s.is_empty()
            && s.split(';')
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        if !ok {
            bail!(
                "`{s}` is not an attribute (bold, dim, italic, underline, blink, reverse, strike) or an SGR parameter list"
            );
        }
        Ok(Attr::Other(s.to_string()))
    }

    fn code(&self) -> &str {
        match self {
            Attr::Other(s) => s,
            named => NAMED_ATTRS
                .iter()
                .find(|(_, attr, _)| attr == named)
                .map(|(_, _, code)| *code)
                .unwrap_or(""),
        }
    }
}

impl<'de> Deserialize<'de> for Attr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Attr::parse(&String::deserialize(d)?).map_err(D::Error::custom)
    }
}

impl Serialize for Attr {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        // The name, not `code()`: `code()` is the SGR parameter list, and a
        // named attr must round-trip through the word it was written as.
        let out = match self {
            Attr::Other(rest) => rest.as_str(),
            named => NAMED_ATTRS
                .iter()
                .find(|(_, attr, _)| attr == named)
                .map(|(name, _, _)| *name)
                .unwrap_or(""),
        };
        s.serialize_str(out)
    }
}

/// A colour in one of the three spaces terminals mean, kept in the form the
/// user wrote so a 256-colour choice survives on a terminal that has truecolour
/// and vice versa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Color {
    /// A bare SGR parameter, 0-255, passed through exactly as written: the
    /// ANSI base colours are 30-37/40-47 and 90-107, whatever the terminal does
    /// with the rest is its business.
    Basic(u8),
    /// 256-colour palette index, `38;5;N`.
    Indexed(u8),
    /// Truecolour, `38;2;R;G;B`, usually from a `#hex`.
    Rgb(u8, u8, u8),
}

impl Color {
    fn parse(s: &str) -> Result<Self> {
        if let Some(hex) = s.strip_prefix('#') {
            let expanded: Vec<u8> = match hex.len() {
                3 => hex.bytes().flat_map(|b| [b, b]).collect(),
                6 => hex.bytes().collect(),
                _ => bail!("`#{hex}` is not a 3- or 6-digit hex colour"),
            };
            let mut rgb = [0u8; 3];
            for (i, pair) in expanded.chunks_exact(2).enumerate() {
                let hi = hex_digit(pair[0])?;
                let lo = hex_digit(pair[1])?;
                rgb[i] = hi << 4 | lo;
            }
            return Ok(Color::Rgb(rgb[0], rgb[1], rgb[2]));
        }
        match s.split(';').collect::<Vec<_>>().as_slice() {
            ["38", "2", r, g, b] => Ok(Color::Rgb(byte(r)?, byte(g)?, byte(b)?)),
            ["38", "5", n] => Ok(Color::Indexed(byte(n)?)),
            [one] => {
                let n: u8 = one.parse().map_err(|_| {
                    anyhow::anyhow!(
                        "`{s}` is not a colour (0-255, `38;5;N`, `38;2;R;G;B` or `#hex`)"
                    )
                })?;
                Ok(Color::Basic(n))
            }
            _ => {
                bail!("`{s}` is not a colour: `#hex`, an ANSI base code, `38;5;N` or `38;2;R;G;B`")
            }
        }
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Color::parse(&String::deserialize(d)?).map_err(D::Error::custom)
    }
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let out = match self {
            Color::Basic(n) => format!("{n}"),
            Color::Indexed(n) => format!("38;5;{n}"),
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        };
        s.serialize_str(&out)
    }
}

/// One styled thing: an optional colour plus any number of text attributes.
///
/// A TOML string is shorthand for a colour alone (`code = "#58a6ff"`); a table
/// is the full form `{ color = …, sgr = ["bold", "italic"] }`, either half
#[derive(Debug, Clone)]
pub struct Style {
    pub color: Option<Color>,
    pub sgr: Vec<Attr>,
    /// The one rendered SGR list behind `codes()`. A Style is immutable once
    /// loaded, while the painted rows re-read it on every frame, so the
    /// rendering is computed once rather than once per use.
    rendered: OnceLock<String>,
}

// `rendered` is a memo of `color`/`sgr` and always agrees with them, so
// equality reads the two fields and leaves the cache out.
impl PartialEq for Style {
    fn eq(&self, other: &Self) -> bool {
        self.color == other.color && self.sgr == other.sgr
    }
}

impl Eq for Style {}

impl Style {
    fn color(c: Color) -> Self {
        Self {
            color: Some(c),
            sgr: Vec::new(),
            rendered: OnceLock::new(),
        }
    }

    fn attrs(a: &[Attr]) -> Self {
        Self {
            color: None,
            sgr: a.to_vec(),
            rendered: OnceLock::new(),
        }
    }

    /// The SGR parameter list this style amounts to, as written between `\x1b[`
    /// and `m` — `"1;3;38;2;88;166;255"`. Empty when the style is bare.
    pub fn codes(&self) -> &str {
        self.rendered.get_or_init(|| {
            let mut out = String::new();
            for a in &self.sgr {
                push_sep(&mut out);
                out.push_str(a.code());
            }
            if let Some(c) = &self.color {
                push_sep(&mut out);
                let _ = match c {
                    Color::Basic(n) => write!(out, "{n}"),
                    Color::Indexed(n) => write!(out, "38;5;{n}"),
                    Color::Rgb(r, g, b) => write!(out, "38;2;{r};{g};{b}"),
                };
            }
            out
        })
    }
}

fn push_sep(out: &mut String) {
    if !out.is_empty() {
        out.push(';');
    }
}

impl<'de> Deserialize<'de> for Style {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Style;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a colour string or a table of `color` and `sgr`")
            }

            fn visit_str<E: serde::de::Error>(self, s: &str) -> std::result::Result<Style, E> {
                Ok(Style {
                    color: Some(Color::parse(s).map_err(E::custom)?),
                    sgr: Vec::new(),
                    rendered: OnceLock::new(),
                })
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Style, A::Error> {
                let mut color: Option<Color> = None;
                let mut sgr: Vec<Attr> = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "color" => color = Some(map.next_value()?),
                        "sgr" => sgr = map.next_value()?,
                        other => return Err(A::Error::unknown_field(other, &["color", "sgr"])),
                    }
                }
                Ok(Style {
                    color,
                    sgr,
                    rendered: OnceLock::new(),
                })
            }
        }
        d.deserialize_any(V)
    }
}

impl Serialize for Style {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        // The shorthand the user wrote — a colour string — only when the
        // sgr list is empty; otherwise the full table. `rendered` never goes.
        use serde::ser::SerializeStruct;
        if self.sgr.is_empty()
            && let Some(c) = &self.color
        {
            return c.serialize(s);
        }
        let mut st = s.serialize_struct("Style", 2)?;
        if let Some(c) = &self.color {
            st.serialize_field("color", c)?;
        }
        st.serialize_field("sgr", &self.sgr)?;
        st.end()
    }
}

/// The SGR behind every Style the terminal uses.
///
/// Keys are grouped by what they style, not by colour: `diff.add` and
/// `status.ok` share a code by default but stay separate so one can change
/// without dragging the other along. `muted`, `heading` and `emphasis` are the
/// text attributes markdown rendering opens; everything else is one Style each.
/// `prompt.icon` is the single value that is neither colour nor attribute.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Theme {
    #[serde(default = "default_muted")]
    pub muted: Style,
    #[serde(default = "default_heading")]
    pub heading: Style,
    #[serde(default = "default_emphasis")]
    pub emphasis: Style,
    #[serde(default = "default_code")]
    pub code: Style,
    #[serde(default)]
    pub diff: Diff,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub menu: Menu,
    #[serde(default)]
    pub prompt: Prompt,
    #[serde(default = "default_input")]
    pub input: Style,
}

const GREEN: Color = Color::Rgb(137, 210, 129);
const RED: Color = Color::Rgb(252, 58, 75);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Diff {
    #[serde(default = "default_add")]
    pub add: Style,
    #[serde(default = "default_del")]
    pub del: Style,
}

impl Default for Diff {
    fn default() -> Self {
        Self {
            add: default_add(),
            del: default_del(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    #[serde(default = "default_ok")]
    pub ok: Style,
    #[serde(default = "default_err")]
    pub err: Style,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            ok: default_ok(),
            err: default_err(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Menu {
    #[serde(default = "default_selected")]
    pub selected: Style,
}

impl Default for Menu {
    fn default() -> Self {
        Self {
            selected: default_selected(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Prompt {
    #[serde(default = "default_prompt_color")]
    pub color: Style,
    #[serde(default = "default_icon")]
    pub icon: String,
    /// What the gutter shows while vim keys are in Normal. Different enough
    /// from `icon` to be read at a glance: the mode is the one thing on
    /// screen that changes what every other key does.
    #[serde(default = "default_normal_icon")]
    pub normal: String,
}

fn default_normal_icon() -> String {
    "\u{00b7}".to_string()
}

impl Default for Prompt {
    fn default() -> Self {
        Self {
            color: default_prompt_color(),
            icon: default_icon(),
            normal: default_normal_icon(),
        }
    }
}

fn default_muted() -> Style {
    Style::attrs(&[Attr::Dim])
}
fn default_heading() -> Style {
    Style::attrs(&[Attr::Bold])
}
fn default_emphasis() -> Style {
    Style::attrs(&[Attr::Italic])
}
fn default_code() -> Style {
    Style::color(Color::Rgb(88, 166, 255))
}
fn default_add() -> Style {
    Style::color(GREEN)
}
fn default_del() -> Style {
    Style::color(RED)
}
fn default_ok() -> Style {
    Style::color(GREEN)
}
fn default_err() -> Style {
    Style::color(RED)
}
fn default_selected() -> Style {
    Style::attrs(&[Attr::Reverse])
}
fn default_prompt_color() -> Style {
    Style::color(Color::Rgb(0, 255, 255))
}

// The input body: the terminal's own foreground until a config colours it.
fn default_input() -> Style {
    Style::attrs(&[])
}

fn default_icon() -> String {
    "›".into()
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            muted: default_muted(),
            heading: default_heading(),
            emphasis: default_emphasis(),
            code: default_code(),
            diff: Diff::default(),
            status: Status::default(),
            menu: Menu::default(),
            prompt: Prompt::default(),
            input: default_input(),
        }
    }
}

fn byte(v: &str) -> Result<u8> {
    v.parse()
        .map_err(|_| anyhow::anyhow!("`{v}` is not a byte (0-255)"))
}

fn hex_digit(b: u8) -> Result<u8> {
    (b as char)
        .to_digit(16)
        .map(|n| n as u8)
        .ok_or_else(|| anyhow::anyhow!("`{}` is not a hex digit", b as char))
}

/// Whether the surface being written to can carry colour, and the theme behind
/// the codes it uses.
#[derive(Debug)]
pub struct Paint {
    pub color: bool,
    pub theme: Arc<Theme>,
}

impl Paint {
    #[cfg(test)]
    pub fn new(color: bool) -> Self {
        Self {
            color,
            theme: Arc::new(Theme::default()),
        }
    }

    pub fn with_theme(color: bool, theme: Arc<Theme>) -> Self {
        Self { color, theme }
    }

    pub fn on(&self, style: &Style, body: &str) -> String {
        if !self.color {
            return body.to_string();
        }
        let codes = style.codes();
        if codes.is_empty() {
            body.to_string()
        } else {
            format!("\x1b[{codes}m{body}{RESET}")
        }
    }
}

/// Terminal styling for the markdown a model writes, decided one line at a
/// time.
///
/// Forward-only, because a line the terminal has printed cannot be restyled:
/// what a row looks like is settled when it ends, out of what came before it.
/// That rules out anything needing the whole document — a table's column
/// widths, a reflowed code block — and leaves what a coding agent actually
/// emits.
#[derive(Debug, Default, Clone, Copy)]
pub struct Markdown {
    /// Inside a ``` block, where nothing is markup and everything is code.
    fenced: bool,
}

impl Markdown {
    /// The line as the terminal should show it.
    ///
    /// Takes `&self`, not `&mut`: the row still being written is styled again
    /// on every frame, and only a line that has ended may decide what the one
    /// after it means.
    pub fn line(&self, text: &str, p: &Paint) -> String {
        if !p.color {
            return text.to_string();
        }
        if fence(text) {
            return p.on(&p.theme.muted, text);
        }
        if self.fenced {
            // A gutter rather than a colour: code has to stay the most legible
            // thing on the screen, and thirty yellow rows is the opposite.
            return format!("{}{text}", p.on(&p.theme.muted, "│ "));
        }
        let body = text.trim_start();
        let pad = &text[..text.len() - body.len()];
        if body.starts_with("> ") {
            // Whole-line, no spans inside: a quote is an aside, and dimming it
            // is the whole of what it needs said.
            return p.on(&p.theme.muted, text);
        }
        if let Some(at) = heading(body) {
            let marker = p.on(&p.theme.muted, &body[..at]);
            let codes = p.theme.heading.codes();
            return format!(
                "{pad}{marker}\x1b[{codes}m{}{RESET}",
                spans(&body[at..], codes, 0, &p.theme)
            );
        }
        match bullet(body) {
            Some(at) => format!(
                "{pad}{}{}",
                p.on(&p.theme.muted, &body[..at]),
                spans(&body[at..], "", 0, &p.theme)
            ),
            None => format!("{pad}{}", spans(body, "", 0, &p.theme)),
        }
    }

    /// A line has ended. A fence is the only thing in it that changes what the
    /// line after it means.
    pub fn advance(&mut self, text: &str) {
        if fence(text) {
            self.fenced = !self.fenced;
        }
    }

    /// A new run starts outside any block, whatever the last one left open.
    pub fn reset(&mut self) {
        self.fenced = false;
    }
}

fn fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

// Where a heading's `#`s and their space end, if the line is one.
//
// The space is what makes it a heading rather than a line that merely opens
// with a hash — which, in a tree full of attributes and shell comments, is
// most of them.
fn heading(body: &str) -> Option<usize> {
    let hashes = body.len() - body.trim_start_matches('#').len();
    ((1..=6).contains(&hashes) && body[hashes..].starts_with(' ')).then_some(hashes + 1)
}

// Where a list item's marker ends, if the line opens with one.
fn bullet(body: &str) -> Option<usize> {
    if body.starts_with("- ") || body.starts_with("* ") || body.starts_with("+ ") {
        return Some(2);
    }
    let digits = body.len() - body.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    (digits > 0 && body[digits..].starts_with(". ")).then_some(digits + 2)
}

// How deep emphasis may hold more emphasis.
//
// Real markdown nests one level, at most two. The bound is not about taste: a
// span recurses on its own body, so a long enough line of `**`*` would put the
// stack in the hands of whatever the model wrote.
const NESTING: u8 = 3;

// The inline spans of one line: code, bold, italic.
// `under` is whatever styling is already open around `text`. A span closes
// with a reset — there is no escape for "bold off" that leaves the rest
// standing — so it has to re-open what it interrupted, or a code span inside
// bold ends the bold at the backtick and the sentence after it goes plain.
//
// Whether a body is literal is a property of the mark, not of how the mark
// happens to be styled: a configured `emphasis` that matches `code` must not
// start swallowing the spans inside it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SpanKind {
    /// Literal all the way down, no markup inside.
    Code,
    /// Emphasis — may hold deeper spans.
    Markup,
}

fn spans(text: &str, under: &str, depth: u8, theme: &Theme) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let Some(at) = rest.find(['`', '*']) else {
            out.push_str(rest);
            break;
        };
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        match span(from, theme) {
            Some((kind, style, body, tail)) => {
                let code = style.codes();
                let inner = if kind == SpanKind::Code || depth == NESTING {
                    // Code span body is embedded raw into the ANSI output.
                    // Escape any literal ESC bytes so they don't inject
                    // spurious SGR sequences into the styled stream.
                    if body.contains('\x1b') {
                        body.replace('\x1b', "[ESC]")
                    } else {
                        body.to_string()
                    }
                } else {
                    let joined = if under.is_empty() {
                        code.to_string()
                    } else {
                        format!("{under};{code}")
                    };
                    spans(body, &joined, depth + 1, theme)
                };
                let reopen = if under.is_empty() {
                    String::new()
                } else {
                    format!("\x1b[{under}m")
                };
                out.push_str(&format!("\x1b[{code}m{inner}{RESET}{reopen}"));
                rest = tail;
            }
            // An opener with no closer is text: the line is still arriving, or
            // the character meant itself.
            None => {
                let mut chars = from.chars();
                out.push(chars.next().unwrap_or_default());
                rest = chars.as_str();
            }
        }
    }
    out
}

// One span at the head of `from`: its code, its text, and what follows it.
//
// `_` is not a delimiter here. It is the word separator of every identifier in
// the tree, and a rule that italicises the middle of `saturating_sub` is worse
// than no italics at all.
fn span<'a, 'b>(
    from: &'a str,
    theme: &'b Theme,
) -> Option<(SpanKind, &'b Style, &'a str, &'a str)> {
    for (mark, kind, style) in [
        ("**", SpanKind::Markup, &theme.heading),
        ("`", SpanKind::Code, &theme.code),
        ("*", SpanKind::Markup, &theme.emphasis),
    ] {
        let Some(rest) = from.strip_prefix(mark) else {
            continue;
        };
        let Some(end) = rest.find(mark) else {
            continue;
        };
        let body = &rest[..end];
        // Flanking, for the emphasis marks only: without it `2 * 3 * 4` reads
        // as an italic 3. A backtick means code wherever it lands.
        let loose = mark != "`"
            && (body.starts_with(char::is_whitespace) || body.ends_with(char::is_whitespace));
        if body.is_empty() || loose {
            continue;
        }
        return Some((kind, style, body, &rest[end + mark.len()..]));
    }
    None
}

/// What a run has cost, in the one wording every place that says it uses.
///
/// The cost is shown only when the model is priced — an unpriced model reports
/// no cost rather than $0.
pub fn spent(usage: &brain::stream::Usage, cost: f64) -> String {
    let mut parts = vec![in_out(usage.input, usage.output)];
    if usage.cache_read > 0 {
        parts.push(format!("{} cached", short(usage.cache_read)));
    }
    if cost > 0.0 {
        parts.push(format!("${cost:.4}"));
    }
    parts.join(" · ")
}

/// The wording for every event that occupies a whole line.
///
/// Both surfaces call this: a tool call has to read the same in a pipe as in
/// the terminal, and two copies of the wording would drift on the first edit.
/// None is the caller's to place: the two deltas, which are a fragment rather
/// than a line, and `Done`, which is a status line the surface composes itself.
/// A run's line for one event, and for a tool that offers one, the rows of
/// detail under it.
///
/// Newline-separated, because the caller decides what a row is: the interactive
/// surface repaints a region and has to hand them over one at a time.
/// The rows a finished tool result takes on screen: its head, clipped to fit,
/// and under it whatever a tool sketched — an edit's diff rows.
///
/// One function, called from both the live stream and the rebuild from the
/// transcript. They used to render this differently, which is what a second
/// producer buys you.
pub fn result_rows(
    is_error: bool,
    name: &str,
    preview: &str,
    p: &Paint,
    width: usize,
) -> Vec<String> {
    let room = width.saturating_sub(2).max(20);
    let mark = if is_error {
        p.on(&p.theme.status.err, "✗")
    } else {
        p.on(&p.theme.status.ok, "✓")
    };
    let (head, rest) = preview.split_once('\n').unwrap_or((preview, ""));
    let mut out = vec![format!(
        "{mark} {name} {}",
        p.on(&p.theme.muted, &clip(head, room))
    )];
    out.extend(rest.lines().map(|row| {
        // The row number leads each diff row, so the mark is the second word;
        // colour beats reading the diff text.
        let style = match row.split_whitespace().nth(1) {
            Some("+") => &p.theme.diff.add,
            Some("-") => &p.theme.diff.del,
            _ => &p.theme.muted,
        };
        p.on(style, &format!("  {}", clip(row, room)))
    }));
    out
}

pub fn describe(event: &Event, p: &Paint, width: usize) -> Option<String> {
    let room = width.saturating_sub(2).max(20);
    Some(match event {
        Event::ToolStart { name, args, .. } => {
            format!(
                "{} {name} {}",
                p.on(&p.theme.muted, "→"),
                p.on(&p.theme.muted, &summarize(args))
            )
        }
        Event::ToolEnd {
            name,
            is_error,
            preview,
            ..
        } => result_rows(*is_error, name, preview, p, width).join("\n"),
        Event::ToolDenied { name, reason, .. } => {
            format!(
                "{} {name} {}",
                p.on(&p.theme.status.err, "✗"),
                p.on(&p.theme.muted, &clip(reason, room))
            )
        }
        Event::Compacted(r) => p.on(&p.theme.muted, &compaction_line(r)),
        Event::Retrying {
            attempt,
            delay_ms,
            reason,
        } => p.on(
            &p.theme.muted,
            &format!("retry {attempt} in {delay_ms}ms · {}", clip(reason, room)),
        ),
        Event::Warning(w) => format!(
            "{} {}",
            p.on(&p.theme.status.err, "!"),
            p.on(&p.theme.muted, w)
        ),
        // Done is a status line rather than an event's wording, and the two
        // surfaces render it from their own configured segments.
        _ => return None,
    })
}

pub struct Renderer {
    paint: Paint,
    quiet: bool,
    /// The segments this surface ends a run with. A pipe times nothing and
    /// queues nothing, so `elapsed` and `queued` have nothing to say here.
    done: Vec<crate::status::Segment>,
    /// Read off the same events the terminal reads, so a piped run ends on the
    /// line the terminal would have shown it.
    tally: crate::status::Tally,
    model: String,
    /// The worktree this run is working in, for the segment that names it.
    worktree: Option<String>,
    thinking: bool,
    /// Each stream is tracked separately: they share a terminal when both are
    /// a tty, but only the dirty one may be terminated when piped apart.
    out_dirty: bool,
    err_dirty: bool,
}

impl Renderer {
    pub fn new(
        quiet: bool,
        theme: Arc<Theme>,
        done: Vec<crate::status::Segment>,
        model: String,
        worktree: Option<String>,
    ) -> Self {
        Self {
            paint: Paint::with_theme(std::io::stderr().is_terminal(), theme),
            quiet,
            done,
            tally: crate::status::Tally::default(),
            model,
            worktree,
            thinking: false,
            out_dirty: false,
            err_dirty: false,
        }
    }

    /// Answer text goes to stdout so it pipes; everything else is progress and
    /// goes to stderr.
    pub fn on(&mut self, event: Event) {
        // Before the arms and outside the `quiet` guards: a run still has to
        // arrive at the right total when nothing about it was printed.
        self.tally.on(&event);
        match &event {
            Event::ReasoningDelta(d) if !self.quiet => {
                if !self.thinking {
                    self.settle_out();
                    eprint!("{}", self.paint.on(&self.paint.theme.muted, "thinking "));
                    self.thinking = true;
                }
                eprint!("{}", self.paint.on(&self.paint.theme.muted, d));
                self.err_dirty = true;
                let _ = std::io::stderr().flush();
            }
            Event::TextDelta(d) => {
                self.end_thinking();
                self.settle_err();
                print!("{d}");
                self.out_dirty = !d.ends_with('\n');
                let _ = std::io::stdout().flush();
            }
            Event::Done { .. } if !self.quiet => {
                self.end_thinking();
                self.settle();
                let snap = self
                    .tally
                    .snapshot(&self.model, self.worktree.as_deref(), None, 0);
                let line = crate::status::line(&self.done, &snap);
                if !line.is_empty() {
                    eprintln!("{}", self.paint.on(&self.paint.theme.muted, &line));
                }
            }
            // Worth seeing even under --quiet: the run did less than it was asked.
            Event::ToolDenied { .. } => {
                self.settle();
                if let Some(line) = describe(&event, &self.paint, 100) {
                    eprintln!("{line}");
                }
            }
            _ if self.quiet => {}
            _ => {
                if let Some(line) = describe(&event, &self.paint, 100) {
                    self.end_thinking();
                    self.settle();
                    eprintln!("{line}");
                }
            }
        }
    }

    fn end_thinking(&mut self) {
        self.thinking = false;
    }

    /// Terminate the answer stream's partial line. Never called between two
    /// text deltas: they continue one line, they do not each start one.
    fn settle_out(&mut self) {
        if self.out_dirty {
            println!();
            self.out_dirty = false;
        }
    }

    fn settle_err(&mut self) {
        if self.err_dirty {
            eprintln!();
            self.err_dirty = false;
        }
    }

    /// Before a whole-line write, which must start at column zero on both.
    fn settle(&mut self) {
        self.settle_out();
        self.settle_err();
    }

    pub fn finish(&mut self) {
        self.end_thinking();
        self.settle();
    }
}

// Says what was given up, not just how much. A silent shrink looks like the
// agent forgetting things for no reason.
fn compaction_line(r: &agent::compact::Report) -> String {
    let mut parts = Vec::new();
    if r.superseded > 0 {
        parts.push(format!("{} superseded", r.superseded));
    }
    if r.uneventful > 0 {
        parts.push(format!("{} uneventful", r.uneventful));
    }
    if r.aged_out > 0 {
        parts.push(format!("{} aged out", r.aged_out));
    }
    if r.args_taken > 0 {
        parts.push(format!("{} arguments taken", r.args_taken));
    }
    if r.dropped > 0 {
        let how = if r.summarized {
            "summarized"
        } else {
            "dropped"
        };
        parts.push(format!("{} messages {how}", r.dropped));
    }
    let detail = if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(", "))
    };
    let warn = if r.still_over {
        " · still over budget"
    } else {
        ""
    };
    format!("compacted {} → {} tokens{detail}{warn}", r.before, r.after)
}

/// One line, cut to `max` columns.
///
/// Columns rather than characters: what overflows a terminal is columns, and a
/// line of Chinese fits half as many characters in the same width. Counting
/// characters let a `grep` pattern or a refusal written in Chinese run to twice
/// the intended width and wrap.
pub fn clip(s: &str, max: usize) -> String {
    let one = s.replace('\n', " ");
    let mut used = 0;
    // An escape is stepped over, not counted: it is a dozen printable
    // characters and zero columns. A cut inside one's reach closes the style.
    let mut esc: Option<Escape> = None;
    let mut styled = false;
    for (i, c) in one.char_indices() {
        if let Some(open) = &mut esc {
            if open.closed(c) {
                esc = None;
            }
            continue;
        }
        if c == '\x1b' {
            (esc, styled) = (Some(Escape::new()), true);
            continue;
        }
        used += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used > max {
            let cut = one[..i].trim_end();
            return match styled {
                true => format!("{cut}{RESET}…"),
                false => format!("{cut}…"),
            };
        }
    }
    one
}

/// Pad a string to a display width with trailing spaces, so a column of
/// mixed-width (CJK) text lines up where `{:width$}` would only count chars.
pub fn pad(s: &str, width: usize) -> String {
    let w = unicode_width::UnicodeWidthStr::width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}

/// The one argument worth showing in a progress line.
pub fn summarize(args: &serde_json::Value) -> String {
    // A patch is many lines; the files it touches are the useful part.
    if let Some(patch) = args.get("patch").and_then(|v| v.as_str()) {
        let files: Vec<&str> = patch
            .lines()
            .filter_map(|l| {
                l.trim()
                    .strip_prefix('[')?
                    .strip_suffix(']')?
                    .rsplit_once('#')
            })
            .map(|(path, _)| path)
            .collect();
        return clip(&files.join(" "), 80);
    }
    // `pattern` before `path`: a grep call carries both, and the pattern is the
    // half that says what the agent was looking for.
    for key in ["pattern", "command", "path", "query"] {
        if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
            return clip(v, 80);
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    /// Every shape of escape prints nothing and costs no columns. Each line
    /// is one the old per-scanner range tests got wrong: a bare `ESC 7` ends
    /// outside a control sequence's byte range, and a control string ends on
    /// a terminator rather than on any range at all.
    #[test]
    fn every_shape_of_escape_costs_no_columns() {
        for (s, text) in [
            ("\u{1b}cab", "ab"),                 // two-byte, final by either range
            ("\u{1b}7ab", "ab"),                 // save cursor: 0x37, the wider range
            ("\u{1b}8ab", "ab"),                 // restore cursor
            ("\u{1b}(Bab", "ab"),                // intermediate 0x28, then final 0x42
            ("\u{1b}[1mab\u{1b}[0m", "ab"),      // the ordinary SGR shape
            ("\u{1b}]0;title\u{7}ab", "ab"),     // OSC closed by BEL
            ("\u{1b}]0;title\u{1b}\\ab", "ab"),  // OSC closed by ST
            ("\u{1b}Pq~~\u{1b}\\ab", "ab"),      // DCS, same terminator
        ] {
            assert_eq!(super::visible_width(s), 2, "{s:?}");
            assert_eq!(super::strip_ansi(s), text, "{s:?}");
        }
    }

    /// Columns, not characters and not bytes — the same measure `clip` and
    /// `fit` take, so a border's column can be spared from a body's width.
    #[test]
    fn visible_width_counts_columns() {
        assert_eq!(super::visible_width("\u{1b}[38;2;0;255;255m\u{258c}\u{1b}[0m "), 2);
        assert_eq!(super::visible_width("\u{4e2d}\u{6587}"), 4);
        assert_eq!(super::visible_width(""), 0);
        // An escape cut off by the end of the string ends the walk rather
        // than looping on it.
        assert_eq!(super::visible_width("ab\u{1b}"), 2);
        assert_eq!(super::visible_width("ab\u{1b}["), 2);
    }

    /// `clip` measures columns. An escape prints nothing, so counting its
    /// bytes cut a painted row to a fraction of the width asked for — the
    /// lane bar lost most of its lanes to a cyan prompt colour.
    #[test]
    fn clip_counts_columns_and_not_the_escapes_between_them() {
        let painted = "\u{1b}[38;2;0;255;255m\u{203a} pi-rs\u{1b}[0m";
        assert_eq!(super::clip(painted, 7), painted, "seven columns fit in seven");
        assert_eq!(super::clip(painted, 40), painted);

        // Plain text is measured exactly as before.
        assert_eq!(super::clip("abcdef", 3), "abc\u{2026}");
        assert_eq!(super::clip("abc", 3), "abc");

        // A cut inside styled text closes the style, or the colour bleeds
        // into whatever the screen draws after this row.
        let cut = super::clip(painted, 3);
        assert!(cut.ends_with("\u{1b}[0m\u{2026}"), "{cut:?}");

        // An escape ends at its own final byte, not at the next `m`. Scanning
        // for `m` alone read `\u{1b}[2K` and everything after it as one escape,
        // measured the row at zero columns, and so never clipped at all.
        assert_eq!(
            super::clip("\u{1b}[2Kabcdef", 3),
            "\u{1b}[2Kabc\u{1b}[0m\u{2026}"
        );

        // CJK still counts two columns a character, escapes or not.
        assert_eq!(super::clip("\u{1b}[2m\u{4f60}\u{597d}\u{1b}[0m", 2), "\u{1b}[2m\u{4f60}\u{1b}[0m\u{2026}");
    }

    use super::{Attr, Color, Markdown, Paint, Style, spent, summarize};
    use brain::stream::Usage;
    use serde_json::json;
    use std::sync::OnceLock;

    fn toml_round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let tree = toml::Value::try_from(value).unwrap();
        T::deserialize(tree).unwrap()
    }

    #[test]
    fn colors_round_trip_through_their_written_forms() {
        assert_eq!(
            toml_round_trip(&Color::Rgb(88, 166, 255)),
            Color::Rgb(88, 166, 255)
        );
        assert_eq!(toml_round_trip(&Color::Indexed(196)), Color::Indexed(196));
        assert_eq!(toml_round_trip(&Color::Basic(31)), Color::Basic(31));
    }

    #[test]
    fn attrs_round_trip_through_their_written_forms() {
        assert_eq!(toml_round_trip(&Attr::Bold), Attr::Bold);
        assert_eq!(
            toml_round_trip(&Attr::Other("8".into())),
            Attr::Other("8".into())
        );
    }

    #[test]
    fn styles_round_trip_both_shapes() {
        let short = Style {
            color: Some(Color::Rgb(88, 166, 255)),
            sgr: Vec::new(),
            rendered: OnceLock::new(),
        };
        assert_eq!(toml_round_trip(&short), short);
        let table = Style {
            color: None,
            sgr: vec![Attr::Bold, Attr::Other("4".into())],
            rendered: OnceLock::new(),
        };
        assert_eq!(toml_round_trip(&table), table);
    }
    #[test]
    fn a_patch_summarizes_to_the_files_it_touches() {
        let patch = "[a.rs#A1B2]\nPUT 1.=1:\n+x\n[b.rs#C3D4]\nRM\n";
        assert_eq!(summarize(&json!({ "patch": patch })), "a.rs b.rs");
    }

    #[test]
    fn consecutive_text_deltas_stay_on_one_line() {
        let mut r = super::Renderer::new(
            false,
            std::sync::Arc::new(super::Theme::default()),
            crate::status::default_done(),
            String::new(),
            None,
        );
        r.on(agent::Event::TextDelta("There".into()));
        assert!(r.out_dirty, "an unterminated delta leaves the line open");
        r.on(agent::Event::TextDelta("'s a bug".into()));
        // settle_out must not fire between deltas, or every token gets its own line.
        assert!(r.out_dirty);
        r.on(agent::Event::TextDelta("done\n".into()));
        assert!(!r.out_dirty, "a delta ending in a newline closes the line");
    }

    #[test]
    fn other_tools_show_their_leading_argument() {
        assert_eq!(summarize(&json!({ "path": "src/a.rs" })), "src/a.rs");
        assert_eq!(summarize(&json!({ "command": "cargo test" })), "cargo test");
        assert_eq!(summarize(&json!({ "nothing": 1 })), "");
    }

    #[test]
    fn a_compaction_line_names_what_was_given_up() {
        let r = agent::compact::Report {
            before: 130_000,
            after: 48_000,
            superseded: 3,
            uneventful: 1,
            aged_out: 6,
            args_taken: 2,
            dropped: 0,
            summarized: false,
            still_over: false,
        };
        assert_eq!(
            super::compaction_line(&r),
            "compacted 130000 → 48000 tokens · 3 superseded, 1 uneventful, 6 aged out, \
             2 arguments taken"
        );
    }

    #[test]
    fn a_summarized_drop_says_so_rather_than_reading_as_a_loss() {
        let r = agent::compact::Report {
            before: 9,
            after: 5,
            dropped: 4,
            summarized: true,
            ..Default::default()
        };
        assert!(super::compaction_line(&r).contains("4 messages summarized"));
    }

    #[test]
    fn a_compaction_that_did_not_fit_says_so() {
        let r = agent::compact::Report {
            before: 9,
            after: 9,
            still_over: true,
            ..Default::default()
        };
        assert!(super::compaction_line(&r).ends_with("still over budget"));
    }

    /// The styling, with the escapes spelled out so a test reads as what the
    /// terminal receives.
    fn md(text: &str) -> String {
        Markdown::default()
            .line(text, &Paint::new(true))
            .replace('\x1b', "^")
    }

    #[test]
    fn emphasis_and_code_are_marked_and_the_delimiters_go() {
        assert_eq!(md("a **b** c"), "a ^[1mb^[0m c");
        assert_eq!(md("a `b` c"), "a ^[38;2;88;166;255mb^[0m c");
        assert_eq!(md("a *b* c"), "a ^[3mb^[0m c");
    }

    #[test]
    fn an_identifier_is_not_emphasis() {
        // `_` is the word separator of every identifier in the tree; a rule
        // that italicises the middle of one is worse than no italics at all.
        assert_eq!(md("call saturating_sub twice"), "call saturating_sub twice");
        // And a lone `*` between spaces is arithmetic, not an opener.
        assert_eq!(md("2 * 3 * 4"), "2 * 3 * 4");
    }

    #[test]
    fn an_opener_with_no_closer_is_text() {
        // The line is still arriving, or the character meant itself.
        assert_eq!(md("what **half a"), "what **half a");
        assert_eq!(md("a `b"), "a `b");
    }

    #[test]
    fn a_heading_needs_its_space() {
        assert_eq!(md("## Why"), "^[2m## ^[0m^[1mWhy^[0m");
        // Otherwise every `#[derive]` and every shell comment is a heading.
        assert_eq!(md("#[derive(Debug)]"), "#[derive(Debug)]");
    }

    #[test]
    fn a_span_inside_emphasis_re_opens_what_it_interrupted() {
        // The model writes this constantly. Without the re-open the bold ends
        // at the backtick and everything after it goes plain.
        assert_eq!(
            md("- **`unwrap()`**：取出"),
            "^[2m- ^[0m^[1m^[38;2;88;166;255munwrap()^[0m^[1m^[0m：取出"
        );
    }

    #[test]
    fn a_deeper_span_joins_the_open_styles_with_semicolons() {
        // Bold holds italic holds code: the reopen after the code span must
        // carry both open attributes, as one `;`-joined parameter list.
        assert_eq!(
            md("**a *b `c`* d**"),
            "^[1ma ^[3mb ^[38;2;88;166;255mc^[0m^[1;3m^[0m^[1m d^[0m"
        );
    }

    #[test]
    fn a_bullet_keeps_its_marker_and_styles_the_rest() {
        assert_eq!(md("- a **b**"), "^[2m- ^[0ma ^[1mb^[0m");
        assert_eq!(md("  1. a"), "  ^[2m1. ^[0ma");
    }

    #[test]
    fn nesting_stops_before_the_stack_does() {
        // A span recurses on its own body; without a bound a long enough line
        // of `**`*` would put the stack in the hands of whatever was written.
        let line = "**".to_string() + &"*a*".repeat(4000) + "**";
        assert!(Markdown::default().line(&line, &Paint::new(true)).len() > line.len());
    }

    #[test]
    fn a_fence_holds_until_the_next_one() {
        let mut m = Markdown::default();
        let p = Paint::new(true);
        assert!(!m.line("fn f() {}", &p).contains('\x1b'), "prose is prose");
        m.advance("```rust");
        // Inside, nothing is markup: a gutter, and the text as written.
        assert_eq!(
            m.line("let a = *b;", &p).replace('\x1b', "^"),
            "^[2m│ ^[0mlet a = *b;"
        );
        m.advance("let a = *b;");
        m.advance("```");
        assert_eq!(
            m.line("done **now**", &p).replace('\x1b', "^"),
            "done ^[1mnow^[0m"
        );
    }

    #[test]
    fn a_plain_surface_is_left_alone() {
        // Piped output is read by something that does not want escapes.
        let out = Markdown::default().line("a **b** `c`", &Paint::new(false));
        assert_eq!(out, "a **b** `c`");
    }

    #[test]
    fn a_search_shows_what_it_looked_for_not_where() {
        let args = json!({ "pattern": "fn tier", "path": "crates/tools/src" });
        assert_eq!(summarize(&args), "fn tier");
    }
    #[test]
    fn a_measured_run_is_the_bill() {
        let usage = Usage {
            input: 8_400,
            output: 390,
            ..Default::default()
        };
        assert_eq!(spent(&usage, 0.0012), "8.4k in / 390 out · $0.0012");
    }

    #[test]
    fn a_part_the_provider_left_out_reads_as_a_dash() {
        // A host that reported no output shows the gap, never a count of ours.
        let usage = Usage {
            input: 8_400,
            ..Default::default()
        };
        assert_eq!(spent(&usage, 0.0), "8.4k in / - out");
    }

    #[test]
    fn a_run_with_nothing_reported_shows_dashes_everywhere() {
        assert_eq!(spent(&Usage::default(), 0.0), "- in / - out");
    }

    // The done line moved to `status`, where both surfaces render it from
    // their own segments; its wording is tested there.
}
