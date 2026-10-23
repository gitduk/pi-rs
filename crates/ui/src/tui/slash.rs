//! What a slash command output, read on the reading page under the
//! transcript — not a transcript notice or a flash, but unread output the
//! user asked for.

use ratatui::text::Line;

use super::screen;
use crate::listing;
use crate::render::Paint;
use pi_store::listing::Listing;

/// What a command answered as lines of at most `width` columns, the key
/// column muted so what a row is reads apart from what it says.
pub fn lines(content: &Listing, width: usize, paint: &Paint) -> Vec<Line<'static>> {
    rows(content, width)
        .into_iter()
        .map(|(key, text)| Line::from(vec![paint.span(&paint.theme.muted, key), text.into()]))
        .collect()
}

// The answer's rows, wrapped to `width`: each as its key column, empty for a
// row with none, and the rest.
fn rows(content: &Listing, width: usize) -> Vec<(String, String)> {
    listing::split(content)
        .into_iter()
        .flat_map(|(head, rest)| {
            let indent = pi_store::text::width(&head);
            // A column too wide to leave the rest half the row wraps whole.
            if indent == 0 || indent * 2 > width {
                return words(&(head + &rest), width)
                    .into_iter()
                    .map(|row| (String::new(), row))
                    .collect::<Vec<_>>();
            }
            if content.brief {
                return vec![(head, pi_store::text::clip(&rest, width - indent))];
            }
            let pad = " ".repeat(indent);
            words(&rest, width - indent)
                .into_iter()
                .enumerate()
                .map(|(at, piece)| {
                    let lead = if at == 0 { head.clone() } else { pad.clone() };
                    (lead, piece)
                })
                .collect()
        })
        .collect()
}

// `text` in rows of at most `width` columns, by the one rule every row on
// screen wraps by.
fn words(text: &str, width: usize) -> Vec<String> {
    let line = Line::from(text.to_string());
    screen::fit(&line, width)
        .iter()
        .map(screen::plain)
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::render::Paint;
    use pi_store::listing::Listing;

    // A line wider than the page wraps at its width, under its key column.
    #[test]
    fn a_long_row_wraps_under_its_key() {
        let output = Listing::say(["x".repeat(30), "last".into()]);
        let lines: Vec<String> = super::lines(&output, 10, &Paint::new(false))
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(lines, ["xxxxxxxxxx", "xxxxxxxxxx", "xxxxxxxxxx", "last"]);
    }
}
