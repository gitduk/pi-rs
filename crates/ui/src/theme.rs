//! A configured theme fitted to the terminal it is drawn on.
//!
//! The config's defaults are for a dark background. Here, and only here, they
//! follow the terminal's: a light one swaps them for Catppuccin Latte, and the
//! input bands lift off whatever the background is. A style the config names
//! keeps its colour and attributes.

use pi_store::theme::{Attr, Bands, Color, PaletteColor, Style, Syntax, Theme};

// Catppuccin Latte's counterparts of the default colours.
const BLUE: Color = PaletteColor::Blue.color(Syntax::CatppuccinLatte);
const TEAL: Color = PaletteColor::Teal.color(Syntax::CatppuccinLatte);
const GREEN: Color = PaletteColor::Green.color(Syntax::CatppuccinLatte);
const RED: Color = PaletteColor::Red.color(Syntax::CatppuccinLatte);
const ACCENT_BLUE: Color = PaletteColor::AccentBlue.color(Syntax::CatppuccinLatte);

/// `theme` fitted to a background, once it is known; unknown leaves it as is.
pub(crate) fn following_terminal(theme: &Theme, bg: Option<(u8, u8, u8)>) -> Theme {
    bg.map_or_else(|| theme.clone(), |bg| for_background(theme, bg))
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

/// The two bands for a terminal whose background is `bg`: opencode's own
/// lift, scaling every channel by one factor so a band keeps the
/// background's hue instead of greying it out.
fn bands_for(bg: (u8, u8, u8)) -> Bands {
    let (r, g, b) = (f64::from(bg.0), f64::from(bg.1), f64::from(bg.2));
    let lum = 0.299 * r + 0.587 * g + 0.114 * b;
    let dark = lum <= 127.5;
    let band = {
        let factor = 3.0 / 12.0;
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
        input: band.clone(),
        said: band,
    }
}
