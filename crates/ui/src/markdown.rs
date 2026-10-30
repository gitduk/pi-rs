//! The embedded syntax palette and the Markdown styles drawn beside it.

use std::io::Cursor;
use std::sync::{Arc, LazyLock};

use pi_store::theme::{Syntax, Theme};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Highlighter, ThemeSet};
use syntect::parsing::{Scope, SyntaxSet};
use syntect::util::LinesWithEndings;
use tui_markdown::{CodeTheme, Options, StyleSheet};

use crate::sgr::style_to_ratatui;

mod document;

pub(crate) fn link_finder() -> &'static linkify::LinkFinder {
    static FINDER: LazyLock<linkify::LinkFinder> = LazyLock::new(|| {
        let mut finder = linkify::LinkFinder::new();
        finder.kinds(&[linkify::LinkKind::Url, linkify::LinkKind::Email]);
        finder
    });
    &FINDER
}

// Kept through parsing, then turned into a copyable language badge.
pub(super) const FENCE: &str = "\u{E000}";

struct Palette {
    code: CodeTheme,
    tokens: syntect::highlighting::Theme,
    headings: [Style; 6],
}

impl Palette {
    fn load(source: &str) -> Self {
        let tokens = ThemeSet::load_from_reader(&mut Cursor::new(source))
            .expect("embedded syntax theme must be valid");
        let highlighter = Highlighter::new(&tokens);
        let headings = std::array::from_fn(|i| {
            // The scope the embedded themes name their heading colours by.
            // `markup.heading.N.markdown` matches only the first two levels,
            // leaving the rest to the terminal's own foreground.
            let scope = Scope::new(&format!("heading.{}.markdown", i + 1))
                .expect("fixed heading scope must be valid");
            let style = highlighter
                .style_mod_for_stack(&[scope])
                .foreground
                .map_or_else(Style::default, |c| {
                    Style::new().fg(Color::Rgb(c.r, c.g, c.b))
                });
            let modifiers = match i {
                0 => Modifier::BOLD | Modifier::UNDERLINED,
                1 => Modifier::BOLD,
                2 => Modifier::BOLD | Modifier::ITALIC,
                _ => Modifier::ITALIC,
            };
            style.add_modifier(modifiers)
        });
        Self {
            code: CodeTheme::from_textmate(source).expect("embedded syntax theme must be valid"),
            tokens,
            headings,
        }
    }

    fn get(syntax: Syntax) -> &'static Self {
        static DARK: LazyLock<Palette> =
            LazyLock::new(|| Palette::load(include_str!("../themes/catppuccin-mocha.tmTheme")));
        static LIGHT: LazyLock<Palette> =
            LazyLock::new(|| Palette::load(include_str!("../themes/catppuccin-latte.tmTheme")));
        match syntax {
            Syntax::Auto | Syntax::CatppuccinMocha => &DARK,
            Syntax::CatppuccinLatte => &LIGHT,
        }
    }
}

/// Shell tokens as foreground colours only. Keep the exact input, including
/// whitespace and incomplete commands, so colouring cannot change layout.
pub(super) fn shell(text: &str, syntax: Syntax) -> Vec<Span<'static>> {
    static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
    let shell = SYNTAXES
        .find_syntax_by_token("sh")
        .expect("bundled syntaxes include shell commands");
    let mut highlighter = HighlightLines::new(shell, &Palette::get(syntax).tokens);
    let mut spans = Vec::new();
    for line in LinesWithEndings::from(text) {
        match highlighter.highlight_line(line, &SYNTAXES) {
            Ok(tokens) => spans.extend(tokens.into_iter().map(|(style, text)| {
                let c = style.foreground;
                Span::styled(text.to_string(), Style::new().fg(Color::Rgb(c.r, c.g, c.b)))
            })),
            Err(_) => spans.push(Span::raw(line.to_string())),
        }
    }
    spans
}

#[derive(Debug, Clone)]
pub(super) struct Sheet {
    theme: Arc<Theme>,
    headings: [Style; 6],
}

impl StyleSheet for Sheet {
    fn heading(&self, level: u8) -> Style {
        self.headings[usize::from(level.clamp(1, 6) - 1)]
    }

    fn heading_marker(&self, _level: u8) -> &str {
        ""
    }

    fn code(&self) -> Style {
        style_to_ratatui(&self.theme.code)
    }

    fn code_block_fence(&self) -> &str {
        FENCE
    }

    fn link(&self) -> Style {
        style_to_ratatui(&self.theme.link)
    }

    fn blockquote(&self) -> Style {
        style_to_ratatui(&self.theme.quote)
    }

    fn heading_meta(&self) -> Style {
        style_to_ratatui(&self.theme.muted)
    }

    fn table_header(&self) -> Style {
        style_to_ratatui(&self.theme.heading)
    }

    fn table_border(&self) -> Style {
        style_to_ratatui(&self.theme.muted)
    }

    fn image_alt(&self) -> Style {
        style_to_ratatui(&self.theme.muted)
    }

    fn alert(&self, kind: tui_markdown::AlertKind) -> Style {
        use tui_markdown::AlertKind;
        let style = match kind {
            AlertKind::Note => &self.theme.link,
            AlertKind::Tip => &self.theme.status.ok,
            AlertKind::Important => &self.theme.quote,
            AlertKind::Warning => &self.theme.status.warn,
            AlertKind::Caution => &self.theme.status.err,
        };
        style_to_ratatui(style)
    }
}

#[derive(Debug)]
pub(super) struct Format {
    sheet: Sheet,
    options: Options<Sheet>,
}

impl Format {
    pub(super) fn render(&self, text: &str, width: Option<usize>) -> Vec<document::Row> {
        document::render(text, self, width)
    }
}

pub(super) fn options(theme: Arc<Theme>) -> Format {
    let palette = Palette::get(theme.syntax);
    // The generic heading style also labels content outside Markdown headings.
    let override_heading = crate::theme::configured_heading(&theme).map(style_to_ratatui);
    let headings = palette
        .headings
        .map(|style| override_heading.map_or(style, |custom| style.patch(custom)));
    let sheet = Sheet { theme, headings };
    Format {
        options: Options::new(sheet.clone()).code_theme(palette.code.clone()),
        sheet,
    }
}

/// The renderer exposes bold/italic attributes but no hooks for their colours.
/// Colour only otherwise uncoloured spans, leaving headings, links and syntax
/// tokens in their own styles.
pub(super) fn inline(style: Style, theme: &Theme) -> Style {
    if style.fg.is_some() {
        return style;
    }
    let mut base = Style::default();
    if style.add_modifier.contains(Modifier::ITALIC) {
        base = base.patch(style_to_ratatui(&theme.emphasis));
    }
    if style.add_modifier.contains(Modifier::BOLD) {
        base = base.patch(style_to_ratatui(&theme.strong));
    }
    base.patch(style)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every level reads as its own, and a light terminal's recoloured generic
    // heading does not read as a config that named one: that would collapse
    // all six onto the single colour the fit chose.
    #[test]
    fn each_heading_level_keeps_a_colour_of_its_own() {
        for bg in [(255, 255, 255), (13, 17, 23)] {
            let theme = crate::theme::following_terminal(&Theme::default(), Some(bg));
            let format = options(Arc::new(theme));
            let mut seen: Vec<_> = (1..=6).map(|l| format.sheet.heading(l).fg).collect();
            let found = seen.len();
            seen.sort_by_key(|c| format!("{c:?}"));
            seen.dedup();
            assert_eq!(seen.len(), found, "{bg:?} shares a colour across levels");
            assert!(
                seen.iter().all(Option::is_some),
                "{bg:?} left a level plain"
            );
        }
    }

    // A heading the config did name still labels every level.
    #[test]
    fn a_named_heading_labels_every_level() {
        let named = pi_store::theme::Color::Rgb(1, 2, 3);
        let mut theme = Theme::default();
        theme.heading = pi_store::theme::Style::color(named.clone());
        let theme = crate::theme::following_terminal(&theme, Some((255, 255, 255)));
        let format = options(Arc::new(theme));
        for level in 1..=6 {
            assert_eq!(format.sheet.heading(level).fg, Some(Color::Rgb(1, 2, 3)));
        }
    }
}
