//! How a config file names a colour, a style and the theme it is made of.
//!
//! The types are a config's, not a painter's: they are what a file is read
//! into, and what a run keeps in force. Turning them into something drawn is
//! the surface's.

use std::fmt::Write as _;
use std::sync::OnceLock;

use anyhow::{Result, bail};
use serde::de::{Error as _, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::icons;

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
    // A bare SGR parameter, 0-255, passed through as written — ANSI base
    // colours are 30-37/40-47 and 90-107; the rest is the terminal's business.
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
/// Shorthand `code = "#58a6ff"`, or the full `{ color = …, sgr = ["bold"] }`,
/// either half optional.
#[derive(Debug, Clone)]
pub struct Style {
    pub color: Option<Color>,
    pub sgr: Vec<Attr>,
    // The rendered SGR list behind `codes()`, memoized since painted rows
    // re-read a style every frame but it never changes after loading.
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
    /// A foreground colour with no additional text attributes.
    pub fn color(c: Color) -> Self {
        Self {
            color: Some(c),
            sgr: Vec::new(),
            rendered: OnceLock::new(),
        }
    }

    /// The terminal's default foreground, dimmed once without a palette colour.
    pub fn dimmed() -> Self {
        Self::color(Color::Basic(39)).adding(Attr::Dim)
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

    /// This style under the pointer: bold added, nothing else — dim stays,
    /// so hover never brightens the row.
    pub fn lit(&self) -> Self {
        self.adding(Attr::Bold)
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

/// The SGR behind every Style the terminal uses, grouped by what a key
/// styles, not by colour, so `diff.add` and `status.ok` can change apart.

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Theme {
    #[serde(default = "default_muted")]
    pub muted: Style,
    #[serde(default = "default_heading")]
    pub heading: Style,
    #[serde(default = "default_emphasis")]
    pub emphasis: Style,
    #[serde(default = "default_strong")]
    pub strong: Style,
    #[serde(default = "default_code")]
    pub code: Style,
    #[serde(default = "default_link")]
    pub link: Style,
    #[serde(default = "default_quote")]
    pub quote: Style,
    #[serde(default)]
    pub syntax: Syntax,
    #[serde(default)]
    pub diff: Diff,
    #[serde(default)]
    pub status: Status,
    #[serde(default)]
    pub menu: Menu,
    /// Where a search's query is found, on the transcript and in its list.
    #[serde(default = "default_match", rename = "match")]
    pub matched: Style,
    /// The one of those the view is on.
    #[serde(default = "default_match_current")]
    pub match_current: Style,
    #[serde(default)]
    pub prompt: Prompt,
    #[serde(default = "default_input")]
    pub input: Style,
    /// A table too wide to draw, read as its source: one per column, in
    /// turn; none leaves the source plain.
    #[serde(default = "default_table")]
    pub table: Vec<Style>,
}

// Catppuccin Mocha's semantic colours; the light counterparts live together
// in `for_background`, so explicitly configured styles stay untouched.
const GREEN: Color = Color::Rgb(166, 227, 161);
const RED: Color = Color::Rgb(243, 139, 168);
const BLUE: Color = Color::Rgb(137, 180, 250);
const TEAL: Color = Color::Rgb(148, 226, 213);
const ACCENT_BLUE: Color = Color::Rgb(99, 168, 248);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Syntax {
    #[default]
    Auto,
    CatppuccinMocha,
    CatppuccinLatte,
}

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
    /// Ran to the end, but what came back deserves a look: a nonzero exit.
    #[serde(default = "default_warn")]
    pub warn: Style,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            ok: default_ok(),
            err: default_err(),
            warn: default_warn(),
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
    /// The alternate prompt prefix. Both prefixes are blank by default.
    #[serde(default = "default_normal_icon")]
    pub normal: String,
    /// The bands for editable and submitted text, the editable one brighter.
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
    Style::dimmed()
}
fn default_heading() -> Style {
    Style::color(BLUE).adding(Attr::Bold)
}
fn default_emphasis() -> Style {
    Style::color(TEAL).adding(Attr::Italic)
}
fn default_strong() -> Style {
    Style::color(GREEN).adding(Attr::Bold)
}
fn default_code() -> Style {
    Style::color(BLUE)
}
fn default_link() -> Style {
    Style::color(BLUE).adding(Attr::Underline)
}
fn default_quote() -> Style {
    Style::color(TEAL)
}
fn default_table() -> Vec<Style> {
    [BLUE, GREEN].into_iter().map(Style::color).collect()
}
fn default_add() -> Style {
    Style::color(GREEN)
}
fn default_del() -> Style {
    Style::color(RED)
}
fn default_ok() -> Style {
    Style::color(Color::Basic(32)).adding(Attr::Bold)
}
fn default_err() -> Style {
    Style::color(Color::Basic(31)).adding(Attr::Bold)
}
fn default_warn() -> Style {
    Style::color(Color::Basic(39)).adding(Attr::Bold)
}
fn default_match() -> Style {
    Style::attrs(&[Attr::Reverse])
}
fn default_match_current() -> Style {
    Style::color(BLUE).adding(Attr::Reverse)
}
fn default_selected() -> Style {
    Style::color(BLUE).adding(Attr::Bold)
}
fn default_prompt_color() -> Style {
    Style::color(ACCENT_BLUE)
}

// Neutral dark bands when the background is unknown.
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
            strong: default_strong(),
            code: default_code(),
            link: default_link(),
            quote: default_quote(),
            syntax: Syntax::default(),
            diff: Diff::default(),
            status: Status::default(),
            menu: Menu::default(),
            matched: default_match(),
            match_current: default_match_current(),
            prompt: Prompt::default(),
            input: default_input(),
            table: default_table(),
        }
    }
}

impl Theme {
    /// Resolve automatic colours against a background. Only defaults follow
    /// it: a style supplied by the config keeps its colour and attributes.
    pub fn for_background(&self, bg: (u8, u8, u8)) -> Self {
        let mut theme = self.clone();
        let defaults = Self::default();
        let light = is_light(bg);
        if theme.syntax == Syntax::Auto {
            theme.syntax = if light {
                Syntax::CatppuccinLatte
            } else {
                Syntax::CatppuccinMocha
            };
        }
        if light {
            for (style, default, color) in [
                (&mut theme.heading, &defaults.heading, (30, 102, 245)),
                (&mut theme.emphasis, &defaults.emphasis, (23, 146, 153)),
                (&mut theme.strong, &defaults.strong, (64, 160, 43)),
                (&mut theme.code, &defaults.code, (30, 102, 245)),
                (&mut theme.link, &defaults.link, (30, 102, 245)),
                (
                    &mut theme.menu.selected,
                    &defaults.menu.selected,
                    (30, 102, 245),
                ),
                (&mut theme.quote, &defaults.quote, (23, 146, 153)),
                (
                    &mut theme.prompt.color,
                    &defaults.prompt.color,
                    (28, 100, 200),
                ),
                (&mut theme.diff.add, &defaults.diff.add, (64, 160, 43)),
                (&mut theme.diff.del, &defaults.diff.del, (210, 15, 57)),
                (
                    &mut theme.match_current,
                    &defaults.match_current,
                    (30, 102, 245),
                ),
            ] {
                if style == default {
                    *style = Style {
                        color: Some(Color::Rgb(color.0, color.1, color.2)),
                        sgr: style.sgr.clone(),
                        rendered: OnceLock::new(),
                    };
                }
            }
            if theme.table == defaults.table {
                theme.table = [(30, 102, 245), (64, 160, 43)]
                    .into_iter()
                    .map(|(r, g, b)| Style::color(Color::Rgb(r, g, b)))
                    .collect();
            }
        }
        if !light && theme.status.warn == defaults.status.warn {
            theme.status.warn = Style::color(Color::Basic(33)).adding(Attr::Bold);
        }
        let bands = bands_for(bg);
        if theme.prompt.panel.input == defaults.prompt.panel.input {
            theme.prompt.panel.input = bands.input;
        }
        if theme.prompt.panel.said == defaults.prompt.panel.said {
            theme.prompt.panel.said = bands.said;
        }
        theme
    }
}

fn is_light((r, g, b): (u8, u8, u8)) -> bool {
    0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b) > 127.5
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
/// lift, scaling every channel by one factor so a band keeps the
/// background's hue instead of greying it out.
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
    use super::{Color, Syntax, Theme};

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

    #[test]
    fn an_explicit_syntax_palette_survives_background_detection_and_roundtrips() {
        let theme: Theme = toml::from_str("syntax = 'catppuccin-mocha'").unwrap();
        let light = theme.for_background((255, 255, 255));
        assert_eq!(light.syntax, Syntax::CatppuccinMocha);
        let saved = toml::to_string(&theme).unwrap();
        assert_eq!(toml::from_str::<Theme>(&saved).unwrap(), theme);
        assert!(toml::from_str::<Theme>("syntax = 'missing'").is_err());
    }
}
