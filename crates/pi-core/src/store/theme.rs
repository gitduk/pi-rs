//! How a config file names a colour, a style and the theme it is made of.
//!
//! The types are a config's, not a painter's: they are what a file is read
//! into, and what a run keeps in force. Turning them into something ratatui
//! draws is `ui::sgr`'s.

use std::fmt::Write as _;
use std::sync::OnceLock;

use anyhow::{Result, bail};
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::store::icons;

#[derive(Debug, Clone, PartialEq, Eq)]
/// One text attribute: bold, dim, italic — whatever SGR can set besides colour.
///
/// `Other` passes a custom parameter list through unchanged ("8" hidden, "21"
/// double underline), for anything the fixed set does not name.
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
            for (i, &[hi, lo]) in expanded.as_chunks::<2>().0.iter().enumerate() {
                rgb[i] = hex_digit(hi)? << 4 | hex_digit(lo)?;
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

    /// This style plus one attribute: hover's bold, a diff's reverse. Built
    /// here, where the fields are, rather than at each of its two callers.
    pub fn adding(&self, attr: Attr) -> Self {
        Self {
            color: self.color.clone(),
            sgr: self.sgr.iter().cloned().chain([attr]).collect(),
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

pub fn push_sep(out: &mut String) {
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
/// `prompt.icon` and `prompt.normal` are the values that are neither colour
/// nor attribute: a sigil is a shape. `prompt.panel.input` and
/// `prompt.panel.said` are `Color`s rather than `Style`s — the bands the
/// prompt sits on, written in the background slot — and they follow the
/// terminal: whatever the config leaves unset is lifted from its background.

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
    /// The bands the prompt paints: the input line, and the lines it lands
    /// as. Two shades, the live one the brighter.
    #[serde(default)]
    pub panel: Bands,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Bands {
    #[serde(default = "default_panel_input")]
    pub input: Color,
    #[serde(default = "default_panel_said")]
    pub said: Color,
}

impl Default for Bands {
    fn default() -> Self {
        Self {
            input: default_panel_input(),
            said: default_panel_said(),
        }
    }
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
            panel: Bands::default(),
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
    // opencode's build agent colour, which is what its input line and its
    // user messages both wear.
    Style::color(Color::Rgb(92, 156, 245))
}

// The bands for a terminal that will not say what its background is:
// opencode's `backgroundPanel` and `backgroundElement`, the live one brighter.
fn default_panel_input() -> Color {
    Color::Rgb(30, 30, 30)
}

fn default_panel_said() -> Color {
    Color::Rgb(20, 20, 20)
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

/// The two bands for a terminal whose background is `bg`: opencode's own
/// lift, every channel scaled by one factor, so a band keeps the background's
/// hue rather than greying it out — which is what makes it read as that canvas
/// lit up instead of as a slab laid over it. A landed line is two twelfths of
/// the way towards white and the live one three; a light background goes the
/// other way, towards black.
pub fn bands_for(bg: (u8, u8, u8)) -> Bands {
    let (r, g, b) = (f64::from(bg.0), f64::from(bg.1), f64::from(bg.2));
    let lum = 0.299 * r + 0.587 * g + 0.114 * b;
    let dark = lum <= 127.5;
    let step = |twelfths: f64| {
        let factor = twelfths / 12.0;
        let scaled = if dark && lum >= 10.0 {
            let ratio = (lum + (255.0 - lum) * factor * 0.4) / lum;
            (r * ratio, g * ratio, b * ratio)
        } else if dark {
            // So close to black that scaling it would leave it black: the
            // band is the lift alone, with no hue of its own to keep.
            let v = factor * 0.4 * 255.0;
            (v, v, v)
        } else if lum > 245.0 {
            let v = 255.0 - factor * 0.4 * 255.0;
            (v, v, v)
        } else {
            let ratio = 1.0 - factor * 0.4;
            (r * ratio, g * ratio, b * ratio)
        };
        let byte = |v: f64| v.clamp(0.0, 255.0).floor() as u8;
        Color::Rgb(byte(scaled.0), byte(scaled.1), byte(scaled.2))
    };
    Bands {
        input: step(3.0),
        said: step(2.0),
    }
}

#[cfg(test)]
mod tests {
    use super::Color;

    #[test]
    fn a_hex_colour_parses_in_both_lengths() {
        assert_eq!(
            Color::parse("#58a6ff").unwrap(),
            Color::Rgb(0x58, 0xa6, 0xff)
        );
        assert_eq!(Color::parse("#fa0").unwrap(), Color::Rgb(0xff, 0xaa, 0x00));
        assert!(Color::parse("#12345g").is_err());
        assert!(Color::parse("#1234").is_err());
    }
}
