//! Folding what a stream prints twice. A failing test run prints the same
//! diff, dump or stack once per failure; past the first copy each one costs
//! the context and says nothing new. A later copy becomes one line naming the
//! copy it repeats, which stays; every other byte is the original's.

use std::collections::HashMap;

use llm::slice::head_bytes;

// Shorter repeats are as likely to be a coincidence of structure — closing
// braces, blank lines — as a block printed twice, and save too little.
const MIN_LINES: usize = 6;
const MIN_BYTES: usize = 200;
// How many earlier copies of a window are tried; bounds the work on output
// that is one line over and over.
const MAX_SOURCES: usize = 16;
// How much of the repeated run's first line the marker quotes.
const QUOTE: usize = 60;

/// `text` with every run of at least six lines that repeats an earlier, kept
/// run byte for byte replaced by one marker line.
pub fn fold_repeats(text: &str) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.len() < MIN_LINES * 2 {
        return text.to_string();
    }
    // Byte offset of each line, and one past the last, so a window of lines
    // is a slice of `text` rather than a joined copy.
    let mut at = Vec::with_capacity(lines.len() + 1);
    let mut offset = 0;
    for line in &lines {
        at.push(offset);
        offset += line.len();
    }
    at.push(offset);
    let window = |start: usize| &text[at[start]..at[start + MIN_LINES]];

    let mut out = String::with_capacity(text.len());
    // Starts of kept windows by their text: where a repeat may point back to.
    let mut sources: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut kept = vec![false; lines.len()];
    // Copies of one run folded back to back: (source, length, copies). Said
    // as one marker, so a run printed forty times costs one line, not forty.
    let mut folding: Option<(usize, usize, usize)> = None;
    let mut streak = 0;
    let mut line = 0;
    while line < lines.len() {
        let mut best = 0;
        let mut from = 0;
        if line + MIN_LINES <= lines.len()
            && let Some(starts) = sources.get(window(line))
        {
            for &start in starts.iter().rev().take(MAX_SOURCES) {
                let mut length = 0;
                while start + length < line
                    && kept[start + length]
                    && line + length < lines.len()
                    && lines[start + length] == lines[line + length]
                {
                    length += 1;
                }
                if length > best {
                    best = length;
                    from = start;
                }
            }
        }
        if best >= MIN_LINES && at[line + best] - at[line] >= MIN_BYTES {
            folding = match folding {
                Some((source, length, copies)) if (source, length) == (from, best) => {
                    Some((source, length, copies + 1))
                }
                other => {
                    say(&mut out, other, &lines);
                    Some((from, best, 1))
                }
            };
            streak = 0;
            line += best;
            continue;
        }
        say(&mut out, folding.take(), &lines);
        out.push_str(lines[line]);
        kept[line] = true;
        // A window becomes a source only once every line of it is known to stay.
        streak += 1;
        if streak >= MIN_LINES {
            let start = line + 1 - MIN_LINES;
            sources.entry(window(start)).or_default().push(start);
        }
        line += 1;
    }
    say(&mut out, folding, &lines);
    out
}

// The marker for `copies` back-to-back copies of the `length` lines at
// `source`, quoting the first of them so the reader can find it above.
fn say(out: &mut String, folded: Option<(usize, usize, usize)>, lines: &[&str]) {
    let Some((source, length, copies)) = folded else {
        return;
    };
    let first = lines[source].trim_end_matches(['\n', '\r']).trim();
    let quote = head_bytes(first, QUOTE);
    let more = if quote.len() < first.len() { "…" } else { "" };
    let what = if copies == 1 {
        format!("{length} lines folded: identical to")
    } else {
        format!("{} lines folded: {copies} copies of", length * copies)
    };
    out.push_str(&format!(
        "[{what} the {length} above that begin “{quote}{more}”]\n"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(tag: &str) -> String {
        (0..8)
            .map(|i| format!("    at {tag}::frame_{i} (src/{tag}.rs:{i}0:5) with enough text\n"))
            .collect()
    }

    // Folding is silent: a line dropped that was not a repeat is one the
    // model never sees and nobody notices is gone.
    #[test]
    fn only_a_verbatim_repeat_of_kept_text_folds() {
        let stack = block("parse");
        let text = format!("FAIL a\n{stack}FAIL b\n{stack}FAIL c\n{}", block("lex"));
        let folded = fold_repeats(&text);
        assert_eq!(
            folded.matches("    at parse::frame_0").count(),
            1,
            "{folded}"
        );
        assert!(
            folded.contains(
                "[8 lines folded: identical to the 8 above that begin “at parse::frame_0"
            )
        );
        assert!(folded.contains("FAIL b\n[8 lines folded"), "{folded}");
        assert!(
            folded.contains(&block("lex")),
            "a different block stays whole"
        );

        // One byte off is not a repeat.
        let near = stack.replacen("frame_3", "frame_X", 1);
        let text = format!("FAIL a\n{stack}FAIL b\n{near}");
        assert_eq!(fold_repeats(&text), text);
    }

    #[test]
    fn copies_back_to_back_are_one_marker() {
        let line = "retrying connection to db.internal:5432 after timeout, attempt pending\n";
        let text = format!("start\n{}end\n", line.repeat(60));
        let folded = fold_repeats(&text);
        assert_eq!(folded.matches("[").count(), 1, "{folded}");
        assert!(
            folded.contains("[54 lines folded: 9 copies of the 6 above"),
            "{folded}"
        );
        assert!(folded.ends_with("end\n"));
    }

    #[test]
    fn short_or_small_repeats_stay() {
        let five: String = (0..5)
            .map(|i| format!("a fairly long line number {i} of the block here\n"))
            .collect();
        let text = format!("{five}x\n{five}");
        assert_eq!(fold_repeats(&text), text);
        let tiny: String = (0..8).map(|i| format!("{i}\n")).collect();
        let text = format!("{tiny}{tiny}");
        assert_eq!(fold_repeats(&text), text);
    }

    // The third copy points at the first, which stays; it is never folded
    // against the second, which is gone.
    #[test]
    fn every_copy_points_at_text_that_stays() {
        let stack = block("io");
        let text = format!("1\n{stack}2\n{stack}3\n{stack}");
        let folded = fold_repeats(&text);
        assert_eq!(folded.matches("[8 lines folded").count(), 2, "{folded}");
        assert_eq!(folded.matches("    at io::frame_7").count(), 1);
    }
}
