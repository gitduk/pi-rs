use std::fmt::Write as _;
use std::io::{IsTerminal, Write};
use std::sync::{Arc, OnceLock};

use agent::Event;
use anyhow::{Result, bail};
use brain::count::{in_out, short};
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::icons;

const RESET: &str = "\x1b[0m";

/// Where an escape sequence ends.
///
/// The three scanners over painted text — `visible` and `clip` here, `fit` in
/// `screen` — differ in what they do with the characters (count their columns,
/// copy them, drop them) but not in where the sequence stops. This decides
/// that for all three. Each used to decide it alone, and they disagreed.
pub struct Escape {
    // What shape it is, once the first character has said. `None` until then.
    kind: Option<Kind>,
    // Inside a control string: the last character was `\x1b`, so a `\` now
    // closes it.
    st: bool,
}

enum Kind {
    // `ESC [`, `ESC O` — parameters and intermediates, then a byte in
    // 0x40-0x7e.
    Control,
    // `ESC P`, `ESC X`, `ESC ]`, `ESC ^`, `ESC _` — DCS, SOS, OSC, PM, APC.
    // Arbitrary text closed by BEL or by ST (`ESC \`), not by any byte
    // range: an OSC setting the window title carries a `;` and the title,
    // and a scanner reading it as a control sequence stops on the first
    // letter and draws the rest of the title.
    Str,
    // A bare `ESC` with an intermediate byte, ending on a byte in 0x30-0x7e
    // — wider than a control sequence's, which is where `ESC 7` lives.
    Bare,
    // Over already: the first character was itself the final byte. `ESC 7`
    // saves the cursor and `ESC 8` restores it — what `less`, `vim` and
    // every progress bar emit most — and 0x37 is outside a control
    // sequence's range, so reading one as a control sequence leaves it
    // looking unfinished and eats the character after it.
    Done,
}

impl Escape {
    /// The state directly after an `\x1b`. Feed it every character that
    /// follows; it answers true on the one that closes the sequence.
    pub fn new() -> Self {
        Self {
            kind: None,
            st: false,
        }
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

// The characters a terminal actually shows, escapes stepped over: they cost
// a dozen bytes and zero columns, so anything measuring or reproducing what
// is on screen has to skip them the same way.
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
    // A known name, or else any non-empty `;`-separated SGR parameter list.
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
    // A bare SGR parameter, 0-255, passed through exactly as written: the
    // ANSI base colours are 30-37/40-47 and 90-107, whatever the terminal does
    // with the rest is its business.
    Basic(u8),
    // 256-colour palette index, `38;5;N`.
    Indexed(u8),
    // Truecolour, `38;2;R;G;B`, usually from a `#hex`.
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
    // The one rendered SGR list behind `codes()`. A Style is immutable once
    // loaded, while the painted rows re-read it on every frame, so the
    // rendering is computed once rather than once per use.
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

/// The style the SGR parameters `codes` writes add up to —
/// `"1;3;38;2;88;166;255"` read back as ratatui sees it.
///
/// Invalid parameters reset the style to default (SGR 0) rather than being
/// ignored, matching the resilience the terminal itself provides.
pub fn parse_sgr(params: &str, mut style: ratatui::style::Style) -> ratatui::style::Style {
    use ratatui::style::{Color as RColor, Modifier as RModifier, Style as RStyle};
    // SGR 30-37 and 90-97 name these eight each, in order.
    const NAMED: [RColor; 8] = [
        RColor::Black,
        RColor::Red,
        RColor::Green,
        RColor::Yellow,
        RColor::Blue,
        RColor::Magenta,
        RColor::Cyan,
        RColor::Gray,
    ];
    const BRIGHT: [RColor; 8] = [
        RColor::DarkGray,
        RColor::LightRed,
        RColor::LightGreen,
        RColor::LightYellow,
        RColor::LightBlue,
        RColor::LightMagenta,
        RColor::LightCyan,
        RColor::White,
    ];
    let mut it = params
        .split(';')
        .map(|p| p.parse::<u8>().unwrap_or(0))
        .peekable();
    while let Some(p) = it.next() {
        match p {
            0 => style = RStyle::default(),
            1 => style = style.add_modifier(RModifier::BOLD),
            2 => style = style.add_modifier(RModifier::DIM),
            3 => style = style.add_modifier(RModifier::ITALIC),
            4 => style = style.add_modifier(RModifier::UNDERLINED),
            5 => style = style.add_modifier(RModifier::SLOW_BLINK),
            6 => style = style.add_modifier(RModifier::RAPID_BLINK),
            7 => style = style.add_modifier(RModifier::REVERSED),
            8 => style = style.add_modifier(RModifier::HIDDEN),
            9 => style = style.add_modifier(RModifier::CROSSED_OUT),
            30..=37 => style = style.fg(NAMED[(p - 30) as usize]),
            39 => style = style.fg(RColor::Reset),
            40..=47 => style = style.bg(NAMED[(p - 40) as usize]),
            49 => style = style.bg(RColor::Reset),
            90..=97 => style = style.fg(BRIGHT[(p - 90) as usize]),
            100..=107 => style = style.bg(BRIGHT[(p - 100) as usize]),
            38 | 48 => {
                let fg = p == 38;
                match it.next() {
                    Some(5) => {
                        let n = it.next().unwrap_or(0);
                        let color = RColor::Indexed(n);
                        style = if fg { style.fg(color) } else { style.bg(color) };
                    }
                    Some(2) => {
                        let r = it.next().unwrap_or(0);
                        let g = it.next().unwrap_or(0);
                        let b = it.next().unwrap_or(0);
                        let color = RColor::Rgb(r, g, b);
                        style = if fg { style.fg(color) } else { style.bg(color) };
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    style
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
    /// What the prompt sigil shows while vim keys are in Normal. The same bar
    /// as `icon` by default — the caret's shape is what says which mode is
    /// up — but its own setting, for a terminal that will not reshape the
    /// caret.
    #[serde(default = "default_normal_icon")]
    pub normal: String,
}

fn default_normal_icon() -> String {
    icons::INPUT_SIGIL_NORMAL.to_string()
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
    icons::INPUT_SIGIL.to_string()
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

    /// `body` in `style`, with bold added while hovered — hover strengthens the
    /// row without changing its colour, so a green check stays green and a grey
    /// body stays grey under the cursor.
    pub fn on_hovered(&self, hovered: bool, style: &Style, body: &str) -> String {
        if !hovered || !self.color {
            return self.on(style, body);
        }
        let bold = Style {
            color: style.color.clone(),
            sgr: style.sgr.iter().cloned().chain([Attr::Bold]).collect(),
            rendered: OnceLock::new(),
        };
        self.on(&bold, body)
    }
}

/// A spend in the one wording every place that says it uses: `/status` hands
/// it a session's figures, and a status line composes the same two out of its
/// `in_out`, `cache` and `cost` segments, which say a run's. The cost is shown
/// only when the model is priced — an unpriced model reports no cost rather
/// than $0.
pub fn spent(t: &agent::Totals) -> String {
    let mut parts = vec![in_out(t.usage.input, t.usage.output)];
    if t.usage.cache_read > 0 {
        parts.push(format!("{} cached", short(t.usage.cache_read)));
    }
    if t.cost > 0.0 {
        parts.push(format!("${:.4}", t.cost));
    }
    parts.join(icons::PART_SEP)
}

/// Render a ratatui Line into an ANSI-escaped string.
pub fn line_to_ansi(line: &ratatui::text::Line<'_>) -> String {
    let mut out = String::new();
    let mut params = String::new();
    let base_style = line.style;
    for span in &line.spans {
        let style = base_style.patch(span.style);
        params.clear();
        append_style_params(&mut params, style);
        if params.is_empty() {
            out.push_str(&span.content);
        } else {
            out.push_str("\x1b[");
            out.push_str(&params);
            out.push('m');
            out.push_str(&span.content);
            out.push_str(RESET);
        }
    }
    out
}

fn write_sgr(out: &mut String, code: u8) {
    push_sep(out);
    let _ = write!(out, "{code}");
}

fn append_color(out: &mut String, color: ratatui::style::Color, bg: bool) {
    use ratatui::style::Color as RColor;
    let base = if bg { 40 } else { 30 };
    let bright = if bg { 100 } else { 90 };
    match color {
        RColor::Reset => write_sgr(out, if bg { 49 } else { 39 }),
        RColor::Black => write_sgr(out, base),
        RColor::Red => write_sgr(out, base + 1),
        RColor::Green => write_sgr(out, base + 2),
        RColor::Yellow => write_sgr(out, base + 3),
        RColor::Blue => write_sgr(out, base + 4),
        RColor::Magenta => write_sgr(out, base + 5),
        RColor::Cyan => write_sgr(out, base + 6),
        // Gray is the eighth named colour, DarkGray the bright black; keeping
        // them apart is what makes SGR 37 survive a row and a block alike.
        RColor::Gray => write_sgr(out, base + 7),
        RColor::DarkGray => write_sgr(out, bright),
        RColor::LightRed => write_sgr(out, bright + 1),
        RColor::LightGreen => write_sgr(out, bright + 2),
        RColor::LightYellow => write_sgr(out, bright + 3),
        RColor::LightBlue => write_sgr(out, bright + 4),
        RColor::LightMagenta => write_sgr(out, bright + 5),
        RColor::LightCyan => write_sgr(out, bright + 6),
        RColor::White => write_sgr(out, bright + 7),
        RColor::Indexed(n) => {
            push_sep(out);
            let prefix = if bg { "48;5;" } else { "38;5;" };
            out.push_str(prefix);
            let _ = write!(out, "{n}");
        }
        RColor::Rgb(r, g, b) => {
            push_sep(out);
            let prefix = if bg { "48;2;" } else { "38;2;" };
            out.push_str(prefix);
            let _ = write!(out, "{r};{g};{b}");
        }
    }
}

fn append_style_params(out: &mut String, style: ratatui::style::Style) {
    use ratatui::style::Modifier as RModifier;
    // Every attribute code `parse_sgr` reads: both ends speak one list.
    for (modifier, code) in [
        (RModifier::BOLD, 1),
        (RModifier::DIM, 2),
        (RModifier::ITALIC, 3),
        (RModifier::UNDERLINED, 4),
        (RModifier::SLOW_BLINK, 5),
        (RModifier::RAPID_BLINK, 6),
        (RModifier::REVERSED, 7),
        (RModifier::HIDDEN, 8),
        (RModifier::CROSSED_OUT, 9),
    ] {
        if style.add_modifier.contains(modifier) {
            write_sgr(out, code);
        }
    }
    if let Some(fg) = style.fg {
        append_color(out, fg, false);
    }
    if let Some(bg) = style.bg {
        append_color(out, bg, true);
    }
}

/// A theme style as ratatui sees it. Through the SGR list, so a bare code —
/// `muted = "2"` — stays the attribute it names rather than becoming a palette
/// entry, and every style renders the same here as in a painted row.
pub fn style_to_ratatui(s: &Style) -> ratatui::style::Style {
    parse_sgr(s.codes(), ratatui::style::Style::default())
}

fn trim_partial_fences(text: &str) -> &str {
    for suffix in ["\n`", "\n``"] {
        if let Some(rest) = text.strip_suffix(suffix)
            && !rest.ends_with('`')
        {
            return rest;
        }
    }
    text
}

#[derive(Debug, Clone)]
struct PiStyleSheet {
    heading: ratatui::style::Style,
    code: ratatui::style::Style,
    muted: ratatui::style::Style,
}

impl tui_markdown::StyleSheet for PiStyleSheet {
    fn heading(&self, _level: u8) -> ratatui::style::Style {
        self.heading
    }

    fn code(&self) -> ratatui::style::Style {
        self.code
    }

    fn link(&self) -> ratatui::style::Style {
        self.code
    }

    fn blockquote(&self) -> ratatui::style::Style {
        self.muted
    }

    fn heading_meta(&self) -> ratatui::style::Style {
        self.muted
    }

    fn table_header(&self) -> ratatui::style::Style {
        self.heading
    }

    fn table_border(&self) -> ratatui::style::Style {
        self.muted
    }

    fn image_alt(&self) -> ratatui::style::Style {
        self.muted
    }
}

/// Parse and render markdown into ANSI-styled lines using `tui-markdown` and theme.
pub fn render_markdown(text: &str, paint: &Paint) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    if !paint.color {
        return text.lines().map(str::to_string).collect();
    }
    let trimmed = trim_partial_fences(text);
    let sheet = PiStyleSheet {
        heading: style_to_ratatui(&paint.theme.heading),
        code: style_to_ratatui(&paint.theme.code),
        muted: style_to_ratatui(&paint.theme.muted),
    };
    let options = tui_markdown::Options::new(sheet);
    let parsed = tui_markdown::from_str_with_options(trimmed, &options);
    parsed.lines.iter().map(line_to_ansi).collect()
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
pub const SKETCH_LIMIT: usize = 24;
/// The preview rows a folded result shows: the head plus the sketch limit.
pub const SKETCHED_ROWS: usize = 1 + SKETCH_LIMIT;

/// The rows a finished tool result takes on screen: its head, clipped to fit,
/// and under it whatever a tool sketched — an edit's diff rows.
pub fn result_rows(
    is_error: bool,
    name: &str,
    preview: &str,
    expanded: bool,
    hovered: bool,
    p: &Paint,
    width: usize,
) -> Vec<String> {
    let room = width.saturating_sub(2).max(20);
    let mark = if is_error {
        p.on(&p.theme.status.err, icons::FAIL_MARK)
    } else {
        p.on(&p.theme.status.ok, icons::DONE_MARK)
    };
    let (head, rest) = preview.split_once('\n').unwrap_or((preview, ""));
    let mut out = vec![format!(
        "{mark} {name} {}",
        p.on(&p.theme.muted, &clip(head, room))
    )];
    let diff_lines: Vec<&str> = rest.lines().collect();
    let diff_style = |row: &str| {
        // The row number leads each diff row, so the mark is the second word;
        // colour beats reading the diff text.
        let style = match row.split_whitespace().nth(1) {
            Some("+") => &p.theme.diff.add,
            Some("-") => &p.theme.diff.del,
            _ => &p.theme.muted,
        };
        p.on(style, &format!("  {}", clip(row, room)))
    };
    let footer = |text: &str| p.on_hovered(hovered, &p.theme.muted, text);
    if diff_lines.len() > SKETCH_LIMIT && !expanded {
        out.extend(diff_lines[..SKETCH_LIMIT].iter().copied().map(&diff_style));
        out.push(footer(&format!(
            "  {} {} more",
            icons::ELLIPSIS,
            diff_lines.len() - SKETCH_LIMIT
        )));
    } else {
        out.extend(diff_lines.iter().copied().map(&diff_style));
        if diff_lines.len() > SKETCH_LIMIT {
            out.push(footer("  ▴ collapse"));
        }
    }
    out
}
fn fmt_delay(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

pub fn describe(event: &Event, p: &Paint, width: usize) -> Option<String> {
    let room = width.saturating_sub(2).max(20);
    Some(match event {
        Event::ToolStart { name, args, .. } => {
            format!(
                "{} {name} {}",
                p.on(&p.theme.muted, icons::PENDING_MARK),
                p.on(&p.theme.muted, &summarize(args))
            )
        }
        Event::ToolEnd {
            name,
            is_error,
            preview,
            ..
        } => result_rows(*is_error, name, preview, false, false, p, width).join("\n"),
        Event::ToolDenied { name, reason, .. } => {
            format!(
                "{} {name} {}",
                p.on(&p.theme.status.err, icons::FAIL_MARK),
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
            &format!(
                "retry {attempt} in {}{}{}",
                fmt_delay(*delay_ms),
                icons::PART_SEP,
                clip(reason, room)
            ),
        ),
        Event::Warning(w) => format!(
            "{} {}",
            p.on(&p.theme.status.err, icons::WARN_MARK),
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
    // The segments this surface ends a run with. A pipe times nothing and
    // queues nothing, so `elapsed` and `queued` have nothing to say here.
    done: Vec<crate::status::Segment>,
    // Read off the same events the terminal reads, so a piped run ends on the
    // line the terminal would have shown it.
    tally: crate::status::Tally,
    model: String,
    // The worktree this run is working in, for the segment that names it.
    worktree: Option<String>,
    thinking: bool,
    // Each stream is tracked separately: they share a terminal when both are
    // a tty, but only the dirty one may be terminated when piped apart.
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

    // Terminate the answer stream's partial line. Never called between two
    // text deltas: they continue one line, they do not each start one.
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

    // Before a whole-line write, which must start at column zero on both.
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

    if r.notices_pruned > 0 {
        parts.push(format!("{} notices pruned", r.notices_pruned));
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
        format!("{}{}", icons::PART_SEP, parts.join(", "))
    };
    let warn = if r.still_over {
        format!("{}still over budget", icons::PART_SEP)
    } else {
        String::new()
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
                true => format!("{cut}{RESET}{}", icons::ELLIPSIS),
                false => format!("{cut}{}", icons::ELLIPSIS),
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
    // Order is priority: `pattern` beats `path` because a grep carries both,
    // and `description`, written for this line, beats the prompt it names.
    for key in [
        "description",
        "pattern",
        "command",
        "path",
        "query",
        "prompt",
        "name",
    ] {
        if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
            return clip(v, 80);
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
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
}
