//! Keep explicit text colours readable while preserving terminal-owned ANSI colours.

use ratatui::style::{Color, Style};

// Minimum contrast for normal-sized text, measured with relative luminance.
const MIN_CONTRAST: f64 = 4.5;

pub(super) fn ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let luminance = |rgb: (u8, u8, u8)| {
        let channel = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(rgb.0) + 0.7152 * channel(rgb.1) + 0.0722 * channel(rgb.2)
    };
    let a = luminance(a);
    let b = luminance(b);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

pub(crate) fn readable(mut style: Style, background: Option<(u8, u8, u8)>) -> Style {
    let rgb = |color| match color {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        Color::Indexed(index @ 16..=231) => {
            let index = index - 16;
            let channel = |n| if n == 0 { 0 } else { 55 + 40 * n };
            Some((
                channel(index / 36),
                channel(index / 6 % 6),
                channel(index % 6),
            ))
        }
        Color::Indexed(index @ 232..=255) => {
            let value = 8 + 10 * (index - 232);
            Some((value, value, value))
        }
        _ => None,
    };
    let background = match style.bg {
        None | Some(Color::Reset) => background,
        Some(color) => rgb(color),
    };
    let Some(background) = background else {
        return style;
    };
    let Some(preferred) = style.fg.and_then(rgb) else {
        return style;
    };
    if ratio(preferred, background) >= MIN_CONTRAST {
        return style;
    }
    let black = (0, 0, 0);
    let white = (255, 255, 255);
    let endpoint = if ratio(black, background) >= ratio(white, background) {
        black
    } else {
        white
    };
    // Search at 8-bit precision, keeping the first colour with enough contrast.
    let blend = |step: u16| {
        let channel = |end: u8, start: u8| {
            ((u16::from(end) * step + u16::from(start) * (255 - step)) / 255) as u8
        };
        (
            channel(endpoint.0, preferred.0),
            channel(endpoint.1, preferred.1),
            channel(endpoint.2, preferred.2),
        )
    };
    let (mut low, mut high) = (1, 255);
    while low < high {
        let mid = (low + high) / 2;
        if ratio(blend(mid), background) >= MIN_CONTRAST {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    let (r, g, b) = blend(low);
    style.fg = Some(Color::Rgb(r, g, b));
    style
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_contrast_colours_become_readable_on_light_and_dark_surfaces() {
        for bg in [(0, 0, 0), (255, 255, 255), (100, 130, 160)] {
            for fg in [
                Color::Rgb(bg.0, bg.1, bg.2),
                Color::Indexed(240),
                Color::Indexed(80),
            ] {
                let style = readable(Style::new().fg(fg).italic(), Some(bg));
                if let Some(Color::Rgb(r, g, b)) = style.fg {
                    assert!(ratio((r, g, b), bg) >= MIN_CONTRAST);
                }
                assert!(
                    style
                        .add_modifier
                        .contains(ratatui::style::Modifier::ITALIC)
                );
                assert_eq!(
                    readable(style, Some(bg)),
                    style,
                    "adjustment must be idempotent"
                );
            }
        }
    }

    #[test]
    fn readable_colours_ansi_colours_and_unknown_backgrounds_are_preserved() {
        for fg in [
            Color::Rgb(255, 255, 255),
            Color::Red,
            Color::Indexed(1),
            Color::Reset,
        ] {
            let style = Style::new().fg(fg);
            assert_eq!(readable(style, Some((0, 0, 0))), style);
            assert_eq!(readable(style, None), style);
        }
        let style = Style::new()
            .fg(Color::Rgb(250, 250, 250))
            .bg(Color::Rgb(255, 255, 255));
        assert_ne!(
            readable(style, Some((0, 0, 0))).fg,
            style.fg,
            "use the painted background"
        );
    }

    #[test]
    fn configured_markdown_colours_are_corrected_without_changing_body_or_rules() {
        use pi_store::theme::{Color as ThemeColor, Style as ThemeStyle, Theme};
        use std::sync::Arc;

        let theme = Theme {
            heading: ThemeStyle::color(ThemeColor::Rgb(8, 8, 8)),
            strong: ThemeStyle::color(ThemeColor::Rgb(8, 8, 8)),
            emphasis: ThemeStyle::color(ThemeColor::Rgb(8, 8, 8)),
            link: ThemeStyle::color(ThemeColor::Rgb(8, 8, 8)),
            code: ThemeStyle::color(ThemeColor::Rgb(8, 8, 8)),
            ..Theme::default()
        };
        let paint = crate::render::Paint::with_theme(true, Arc::new(theme))
            .with_background(Some((0, 0, 0)));
        let rows = crate::render::render_markdown(
            "plain\n\n# Heading\n\n**bold** *italic* [link](https://example.com) `code`",
            &paint,
        );
        for word in ["Heading", "bold", "italic", "link", "code"] {
            let span = rows
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content == word)
                .expect("styled word should be rendered");
            let Some(Color::Rgb(r, g, b)) = span.style.fg else {
                panic!("{word} should have a foreground");
            };
            assert!(ratio((r, g, b), (0, 0, 0)) >= MIN_CONTRAST, "{word}");
        }
        let body = rows
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == "plain")
            .expect("body should be rendered");
        assert_eq!(body.style.fg, None, "plain body follows the terminal");
        let rule = crate::theme::table_separator(Some((255, 255, 255)), Some((0, 0, 0)));
        assert_eq!(rule.fg, Some(Color::Rgb(51, 51, 51)));
    }
}
