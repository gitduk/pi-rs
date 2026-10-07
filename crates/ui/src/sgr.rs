//! A theme's styles as ratatui draws them.

use pi_store::theme::{Color, Style};

/// A theme style as ratatui sees it. Through the SGR list, so a bare code —
/// `muted = "2"` — stays the attribute it names rather than becoming a palette
/// entry, and every style renders the same here as in a painted row.
pub fn style_to_ratatui(s: &Style) -> ratatui::style::Style {
    parse_sgr(s.codes(), ratatui::style::Style::default())
}

/// The band a panel colour paints: the same colour written in the background
/// slot, through the same parser, so a bare code — SGR's `49` — means the same
/// here as it does anywhere else.
pub fn band_to_ratatui(c: &Color) -> ratatui::style::Style {
    let params = match c {
        Color::Basic(n) => n.to_string(),
        Color::Indexed(n) => format!("48;5;{n}"),
        Color::Rgb(r, g, b) => format!("48;2;{r};{g};{b}"),
    };
    parse_sgr(&params, ratatui::style::Style::default())
}

use ratatui::style::Color as RColor;

// SGR 30-37 and 90-97 name these eight each, in order. Gray is the eighth
// named colour and DarkGray the bright black, so SGR 37 survives a round trip.
pub(crate) const NAMED: [RColor; 8] = [
    RColor::Black,
    RColor::Red,
    RColor::Green,
    RColor::Yellow,
    RColor::Blue,
    RColor::Magenta,
    RColor::Cyan,
    RColor::Gray,
];
pub(crate) const BRIGHT: [RColor; 8] = [
    RColor::DarkGray,
    RColor::LightRed,
    RColor::LightGreen,
    RColor::LightYellow,
    RColor::LightBlue,
    RColor::LightMagenta,
    RColor::LightCyan,
    RColor::White,
];

/// The style the SGR parameters `codes` writes add up to —
/// `"1;3;38;2;88;166;255"` read back as ratatui sees it.
///
/// Invalid parameters reset the style to default (SGR 0) rather than being
/// ignored, matching the resilience the terminal itself provides.
pub fn parse_sgr(params: &str, mut style: ratatui::style::Style) -> ratatui::style::Style {
    use ratatui::style::{Color as RColor, Modifier as RModifier, Style as RStyle};
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
