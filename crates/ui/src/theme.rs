//! A configured theme fitted to the terminal it is drawn on.
//!
//! The config's defaults are for a dark background. Here, and only here, they
//! follow the terminal's: a light one swaps them for Catppuccin Latte, and the
//! input bands lift off whatever the background is. A style the config names
//! keeps its colour and attributes.

mod contrast;

pub(crate) use contrast::readable;

use pi_store::theme::{Attr, Bands, Color, PaletteColor, Style, Syntax, Theme};

// Catppuccin Latte's counterparts of the default colours.
const BLUE: Color = PaletteColor::Blue.color(Syntax::CatppuccinLatte);
const TEAL: Color = PaletteColor::Teal.color(Syntax::CatppuccinLatte);
const GREEN: Color = PaletteColor::Green.color(Syntax::CatppuccinLatte);
const RED: Color = PaletteColor::Red.color(Syntax::CatppuccinLatte);
const ACCENT_BLUE: Color = PaletteColor::AccentBlue.color(Syntax::CatppuccinLatte);

/// Fit default styles to the terminal's colours, preserving configured styles.
pub(crate) fn following_terminal(
    theme: &Theme,
    bg: Option<(u8, u8, u8)>,
    fg: Option<(u8, u8, u8)>,
) -> Theme {
    let mut fitted = bg.map_or_else(|| theme.clone(), |bg| for_background(theme, bg));
    if theme.muted == Theme::default().muted {
        fitted.muted = secondary_text(fg, bg);
    }
    if theme.strong == Theme::default().strong
        && let Some(bg) = bg
        && let Some(Color::Rgb(r, g, b)) = fitted.strong.color
    {
        // A small amount of the canvas softens the default emphasis colour.
        const EMPHASIS_WEIGHT: u16 = 90;
        let (r, g, b) = blend((r, g, b), bg, EMPHASIS_WEIGHT);
        fitted.strong = fitted.strong.recolored(Color::Rgb(r, g, b));
    }
    fitted
}

/// Use a measured contrast for secondary text when both terminal colours are known.
pub(crate) fn secondary_text(
    foreground: Option<(u8, u8, u8)>,
    background: Option<(u8, u8, u8)>,
) -> Style {
    // Keep 60% of the foreground's contrast without terminal-dependent dimming.
    const FOREGROUND_WEIGHT: u16 = 60;
    foreground
        .zip(background)
        .map_or_else(Style::dimmed, |(fg, bg)| {
            let (r, g, b) = blend(fg, bg, FOREGROUND_WEIGHT);
            let style = readable(
                ratatui::style::Style::new().fg(ratatui::style::Color::Rgb(r, g, b)),
                Some(bg),
            );
            match style.fg {
                Some(ratatui::style::Color::Rgb(r, g, b)) => Style::color(Color::Rgb(r, g, b)),
                _ => unreachable!("an RGB foreground stays RGB"),
            }
        })
}

/// The heading style the config set itself, if it did: an unset one lets each
/// Markdown heading level keep the syntax palette's own colour.
pub(crate) fn configured_heading(theme: &Theme) -> Option<&Style> {
    let default = Theme::default().heading;
    let light = default.recolored(BLUE);
    (theme.heading != default && theme.heading != light).then_some(&theme.heading)
}

pub(crate) fn blend(
    foreground: (u8, u8, u8),
    background: (u8, u8, u8),
    weight: u16,
) -> (u8, u8, u8) {
    let channel =
        |fg: u8, bg: u8| ((u16::from(fg) * weight + u16::from(bg) * (100 - weight)) / 100) as u8;
    (
        channel(foreground.0, background.0),
        channel(foreground.1, background.1),
        channel(foreground.2, background.2),
    )
}

/// Decorative rules retain only 20% of the text's contrast against the canvas.
pub(crate) fn table_separator(
    foreground: Option<(u8, u8, u8)>,
    background: Option<(u8, u8, u8)>,
) -> ratatui::style::Style {
    use ratatui::style::{Color, Modifier, Style};
    match foreground.zip(background) {
        Some((fg, bg)) => {
            let (r, g, b) = blend(fg, bg, 20);
            Style::new().fg(Color::Rgb(r, g, b))
        }
        None => Style::new().add_modifier(Modifier::DIM),
    }
}

fn for_background(theme: &Theme, bg: (u8, u8, u8)) -> Theme {
    let mut theme = theme.clone();
    let defaults = Theme::default();
    let light = is_light(bg);
    if theme.syntax == Syntax::Auto {
        theme.syntax = if light {
            Syntax::CatppuccinLatte
        } else {
            Syntax::CatppuccinMocha
        };
    }
    if light {
        recolor_defaults(&mut theme, &defaults);
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

// Keep explicit styles; replace only colours inherited from the dark defaults.
fn recolor_defaults(theme: &mut Theme, defaults: &Theme) {
    for (style, default, color) in [
        (&mut theme.heading, &defaults.heading, BLUE),
        (&mut theme.emphasis, &defaults.emphasis, TEAL),
        (&mut theme.strong, &defaults.strong, GREEN),
        (&mut theme.code, &defaults.code, BLUE),
        (&mut theme.link, &defaults.link, BLUE),
        (&mut theme.menu.selected, &defaults.menu.selected, BLUE),
        (&mut theme.quote, &defaults.quote, TEAL),
        (&mut theme.prompt.color, &defaults.prompt.color, ACCENT_BLUE),
        (&mut theme.diff.add, &defaults.diff.add, GREEN),
        (&mut theme.diff.del, &defaults.diff.del, RED),
        (&mut theme.match_current, &defaults.match_current, BLUE),
    ] {
        if style == default {
            *style = style.recolored(color);
        }
    }
    if theme.table == defaults.table {
        theme.table = [BLUE, GREEN].into_iter().map(Style::color).collect();
    }
}

fn is_light((r, g, b): (u8, u8, u8)) -> bool {
    0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b) > 127.5
}

/// Neutral fills distinguish the editor and submitted text from the terminal canvas.
fn bands_for(bg: (u8, u8, u8)) -> Bands {
    // Light surfaces need only a hint of black; dark surfaces lift toward white.
    let (overlay, input_weight, said_weight) = if is_light(bg) {
        ((0, 0, 0), 4, 2)
    } else {
        ((255, 255, 255), 12, 16)
    };
    let fill = |weight| {
        let (r, g, b) = blend(overlay, bg, weight);
        Color::Rgb(r, g, b)
    };
    Bands {
        input: fill(input_weight),
        said: fill(said_weight),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_fills_soften_tinted_backgrounds_and_preserve_configured_bands() {
        for bg in [(13, 17, 23), (240, 245, 250)] {
            let fitted = following_terminal(&Theme::default(), Some(bg), None);
            for color in [&fitted.prompt.panel.input, &fitted.prompt.panel.said] {
                let Color::Rgb(r, g, b) = *color else {
                    panic!("a known background should produce an RGB fill");
                };
                assert!(
                    b - r <= bg.2 - bg.0,
                    "neutral mixing must not amplify the tint"
                );
                for (value, original) in [(r, bg.0), (g, bg.1), (b, bg.2)] {
                    if is_light(bg) {
                        assert!(value < original, "light fills darken the canvas gently");
                    } else {
                        assert!(value > original, "dark fills lift off the canvas");
                    }
                }
            }
            let mut configured = Theme::default();
            configured.prompt.panel.input = Color::Rgb(1, 2, 3);
            configured.prompt.panel.said = Color::Rgb(4, 5, 6);
            assert_eq!(
                following_terminal(&configured, Some(bg), None).prompt.panel,
                configured.prompt.panel
            );
        }
    }

    #[test]
    fn secondary_text_follows_light_and_dark_terminals_without_dimming_twice() {
        for (fg, bg) in [((200, 200, 200), (0, 0, 0)), ((0, 0, 0), (200, 200, 200))] {
            let theme = following_terminal(&Theme::default(), Some(bg), Some(fg));
            let Some(Color::Rgb(r, g, b)) = theme.muted.color else {
                panic!("known terminal colours should produce an explicit foreground");
            };
            for (value, foreground, background) in
                [(r, fg.0, bg.0), (g, fg.1, bg.1), (b, fg.2, bg.2)]
            {
                assert!(value > foreground.min(background) && value < foreground.max(background));
            }
            assert!(
                theme.muted.sgr.is_empty(),
                "the colour already reduces contrast"
            );
        }
    }

    #[test]
    fn missing_colours_keep_the_fallback_and_configured_muted_styles_survive() {
        let fg = Some((200, 210, 220));
        let bg = Some((10, 20, 30));
        for (fg, bg) in [(None, bg), (fg, None), (None, None)] {
            assert_eq!(secondary_text(fg, bg), Style::dimmed());
        }
        let theme = Theme {
            muted: Style::color(Color::Rgb(70, 80, 90)).adding(Attr::Italic),
            ..Theme::default()
        };
        assert_eq!(following_terminal(&theme, bg, fg).muted, theme.muted);
    }
}
