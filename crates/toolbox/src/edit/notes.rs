//! What the model is told beyond the diff: why a landing broke the syntax,
//! which block a landing covered, and a deletion that left a blank row.

use super::crop;
use super::edits::{Applied, Landed};
use super::sketch::hunk_rows;
use crate::blocks::by_row;

// The syntax break an edit left behind, and where to look. Flags only
// "parsed before, not now": a broken file is often why the edit ran.
pub(super) fn broke_syntax(
    path: &str,
    before: &str,
    after: &str,
    landed: &[Landed],
) -> Option<String> {
    let rows = crate::parses::broke_rows(path, Some(before), after);
    let row = nearest_row(&rows, landed)?;
    let text = crate::parses::row_text(after, row);
    let mut why = format!(
        "{path} would not parse: it did before this edit, and line {row} of what this one \
         produces is `{text}`. An anchor that covers a line too few or too many does exactly \
         this. Re-read and check where the block actually ends. Nothing was written."
    );
    why.push('\n');
    why.push_str(&hunk_help(before, after, landed));
    Some(why)
}

// Which break to name first: the one closest to a line this edit wrote. A
// stray brace makes the whole file one error node opening on row 1.
fn nearest_row(rows: &[usize], landed: &[Landed]) -> Option<usize> {
    if landed.is_empty() {
        return rows.first().copied();
    }
    let distance = |row: &usize| {
        landed
            .iter()
            .map(|l| {
                let (lo, hi) = (l.start.min(l.end), l.start.max(l.end));
                row.saturating_sub(hi).max(lo.saturating_sub(*row))
            })
            .min()
            .unwrap_or(0)
    };
    rows.iter()
        .min_by_key(|row| (distance(row), **row))
        .copied()
}

// What the landings point at, for a break a bare "line N is `}`" leaves the
// model to hunt: each hunk's displaced lines, and any body that nets otherwise.
fn hunk_help(before: &str, after: &str, landed: &[Landed]) -> String {
    // Hunks spelt out before the rest are summarised.
    const SHOWN: usize = 6;
    let new: Vec<&str> = after.lines().collect();
    let old: Vec<&str> = before.lines().collect();
    let mut out = String::from("The hunks, against the file as it stands:");
    let mut off = String::new();
    let shown = llm::figures::shown(landed.len(), SHOWN);
    for l in landed.iter().take(shown) {
        // The model's own numbering: nothing was written, so `took_at` is the
        // line the anchor sits on, where `start` is where it would have landed.
        let at = l.took_at;
        let took_len = l.took.len().saturating_sub(1);
        let addr = if took_len == 0 {
            format!("{at}")
        } else {
            format!("{at}-{}", at + took_len)
        };
        if l.took.is_empty() {
            out.push_str(&format!("\n  {addr}(insertion)"));
        } else {
            let cur = l
                .took
                .iter()
                .map(|s| crop(s, 60))
                .collect::<Vec<_>>()
                .join("\n    ");
            out.push_str(&format!("\n  {addr}: `{cur}`"));
        }
        let took: isize = l.took.iter().map(|s| brace_net(s)).sum();
        let gave: isize = hunk_rows(&new, l).iter().map(|s| brace_net(s)).sum();
        if took != gave {
            let mut line =
                format!("\n  {addr}: its body nets {gave}, the lines it displaces net {took}");
            // Where the construct the anchor opened at actually ends, read off
            // the file the model will address: the extent it got wrong, named.
            if let Some(e) = balanced_end(&old, at) {
                if e > at {
                    line.push_str(&format!(
                        "; it opens at {at} and balances at line {e} — cover to {e}"
                    ));
                } else {
                    // The displaced lines never opened a brace the body fails
                    // to close: the hunk is the problem, not the extent.
                    line.push_str(&format!(
                        "; the imbalance sits at line {e} — replace or cut it"
                    ));
                }
            }
            off.push_str(&line);
        }
    }
    if let Some(rest) = landed.len().checked_sub(shown).filter(|n| *n > 0) {
        out.push_str(&format!("\n  {rest} more"));
    }
    if !off.is_empty() {
        out.push_str("\nBrace balance:");
        out.push_str(&off);
    }
    out
}

fn brace_net(s: &str) -> isize {
    s.chars().fold(0, |n, c| match c {
        '{' => n + 1,
        '}' => n - 1,
        _ => n,
    })
}

// The first line at or after `start` where the running brace count stops
// being positive — where the construct that opens there actually ends.
fn balanced_end(lines: &[&str], start: usize) -> Option<usize> {
    let mut net = 0isize;
    for (i, l) in lines.iter().enumerate().skip(start.saturating_sub(1)) {
        net += brace_net(l);
        if net <= 0 {
            return Some(i + 1);
        }
    }
    None
}

// What the model is told beyond the sketch: landings that took a whole block
// when only a line was meant — the extent it got wrong, named.
pub(super) fn notes(path: &str, before: &str, applied: &Applied) -> String {
    let old: Vec<&str> = before.lines().collect();
    let extents = by_row(path, before);
    let mut out = String::new();
    for l in &applied.landed {
        let Some((cs, ce)) = extents.get(&l.took_at).copied() else {
            continue;
        };
        let Some(covered) = old.get(cs - 1..ce) else {
            continue;
        };
        if ce > cs
            && l.took.len() == covered.len()
            && l.took.iter().zip(covered).all(|(t, w)| t == w)
        {
            out.push_str(&format!(
                "  covered the block at lines {cs}-{ce}: `{}`\n",
                crop(covered[0], 60)
            ));
        }
    }
    out
}

// The note a deletion earns when it emptied a line and left the row standing.
pub(super) fn blank_note(edits: &[usize]) -> String {
    let named: Vec<String> = edits.iter().map(|i| format!("edits[{i}]")).collect();
    let said = if edits.len() == 1 {
        "emptied a line but left the line break, so a blank row stands where it was. \
         Include the trailing newline in the anchor to take the row with it."
    } else {
        "emptied the lines but left the line breaks, so blank rows stand where they \
         were. Include the trailing newline in the anchor to take the rows with them."
    };
    format!("\n{} {said}\n", named.join(", "))
}
