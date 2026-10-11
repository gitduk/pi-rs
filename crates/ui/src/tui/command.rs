//! What a slash command output, read on the reading page under the
//! transcript — not a transcript notice or a flash, but unread output the
//! user asked for.

use ratatui::text::Line;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
            let indent = UnicodeWidthStr::width(head.as_str());
            // A column too wide to leave the rest half the row wraps whole.
            if indent == 0 || indent * 2 > width {
                return words(&(head + &rest), width)
                    .into_iter()
                    .map(|row| (String::new(), row))
                    .collect::<Vec<_>>();
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

// Break `text` into rows of at most `width` columns, at spaces where it can;
// a word wider than a row is cut where the row ends. A line that fits is kept
// as it is, runs of spaces and all.
fn words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in text.split('\n') {
        if UnicodeWidthStr::width(line) <= width {
            out.push(line.to_string());
            continue;
        }
        let mut row = String::new();
        let mut used = 0;
        for word in line.split(' ').filter(|w| !w.is_empty()) {
            let w = UnicodeWidthStr::width(word);
            if used > 0 && (used + 1 + w > width && w <= width || used + 1 >= width) {
                out.push(std::mem::take(&mut row));
                used = 0;
            }
            if used > 0 {
                row.push(' ');
                used += 1;
            }
            for c in word.chars() {
                let cw = UnicodeWidthChar::width(c).unwrap_or(0);
                if used + cw > width && used > 0 {
                    out.push(std::mem::take(&mut row));
                    used = 0;
                }
                row.push(c);
                used += cw;
            }
        }
        out.push(row);
    }
    out
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
