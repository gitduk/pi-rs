//! Byte-anchored edits: the surface the edit tool speaks.
//!
//! An anchor is literal text found in the file; a landing is the byte range
//! it maps to, and nothing writes unless every edit resolves against the
//! original content. Lines matter only for a block's first line and the line
//! numbers reported back.

use super::crop;
use std::collections::BTreeMap;
use std::ops::Range;

/// Where one edit landed, for the report the model reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    /// Where the new lines sit in the file now.
    pub start: usize,
    pub end: usize,
    /// The original lines it displaced, in order. Empty for an insertion.
    pub took: Vec<String>,
    /// The 1-based line this edit acted on, in the content it resolved
    /// against: for an insertion, the line it landed on.
    pub took_at: usize,
}

/// What an edit does where its anchor lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Anchor {
    /// Replace the matched text.
    Replace(String),
    /// Insert the new text immediately after the match's last byte.
    After(String),
    /// Insert it immediately before the match's first byte.
    Before(String),
}

impl Anchor {
    fn text(&self) -> &str {
        match self {
            Anchor::Replace(t) | Anchor::After(t) | Anchor::Before(t) => t,
        }
    }
}

/// One edit: what to look for, what to put there, and how far to read the look.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub anchor: Anchor,
    pub new: String,
    /// Take every occurrence instead of refusing an anchor that names many.
    pub replace_all: bool,
    /// The anchor names the first line of a block; the whole block is meant.
    pub whole_block: bool,
}

/// The file after the edits, where each of them landed, and the edits whose
/// work left a blank line behind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub content: String,
    pub landed: Vec<Landed>,
    /// The edits that emptied a line without taking its break, so a blank row
    /// stands where it was: `new_string: ""` on an anchor ending at end-of-line.
    pub left_blank: Vec<usize>,
}

impl Refusal {
    /// The stable tag a refusal is logged under, whatever its prose says.
    pub fn tag(&self) -> &'static str {
        match self {
            Refusal::NoEdits => "NO_EDITS",
            Refusal::EmptyAnchor { .. } => "EMPTY_ANCHOR",
            Refusal::EmptyInsert { .. } => "EMPTY_INSERT",
            Refusal::NoMatch { .. } => "NO_MATCH",
            Refusal::Ambiguous { .. } => "AMBIGUOUS",
            Refusal::Overlap { .. } => "OVERLAP",
            Refusal::InsertInside { .. } => "INSERT_INSIDE",
            Refusal::NoBlock { .. } => "NO_BLOCK",
            Refusal::AmbiguousBlock { .. } => "AMBIGUOUS_BLOCK",
            Refusal::Unchanged { .. } => "UNCHANGED",
        }
    }
}

/// Why a call was refused. Every message is read by the model, so each one says
/// what to do next.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("no edits: name at least one replacement")]
    NoEdits,

    #[error("in {path}, edits[{index}]: the anchor is empty — name the text to find")]
    EmptyAnchor { path: String, index: usize },

    #[error("in {path}, edits[{index}]: inserting nothing changes nothing")]
    EmptyInsert { path: String, index: usize },

    #[error(
        "no match in {path} for edits[{index}] — the closest real line is {line}: `{text}`. \
         Nothing was written."
    )]
    NoMatch {
        path: String,
        index: usize,
        line: usize,
        text: String,
    },

    #[error(
        "edits[{index}] matches {n} places in {path}:\n{detail}\n\
         Give more of the anchor to name one, or set `replace_all`."
    )]
    Ambiguous {
        path: String,
        index: usize,
        n: usize,
        detail: String,
    },

    #[error(
        "edits[{a}] and edits[{b}] overlap in {path} — merge them into one \
         edit or target disjoint regions"
    )]
    Overlap { path: String, a: usize, b: usize },

    #[error(
        "edits[{a}] and edits[{b}] meet at one byte in {path} — the order would \
         be a guess; merge them into one edit"
    )]
    InsertInside { path: String, a: usize, b: usize },

    #[error("in {path}, edits[{index}]: no block opens with `{text}`{available}")]
    NoBlock {
        path: String,
        index: usize,
        text: String,
        available: String,
    },

    #[error(
        "in {path}, edits[{index}]: `{text}` opens {n} blocks — give more of \
         the opening line"
    )]
    AmbiguousBlock {
        path: String,
        index: usize,
        text: String,
        n: usize,
    },

    #[error("{path}: nothing changed — the edits reproduce what is already there")]
    Unchanged { path: String },
}

/// Apply every edit to `content`, each matched against the original.
///
/// A `whole_block` anchor resolves through [`crate::blocks`], which reads the
/// outline tree-sitter gives the file.
pub fn apply(path: &str, content: &str, edits: &[Edit]) -> Result<Applied, Refusal> {
    if edits.is_empty() {
        return Err(Refusal::NoEdits);
    }
    // A byte-order mark is invisible to the model, so it is never part of an
    // anchor: match without it and write it back.
    let (bom, body) = split_bom(content);
    let crlf = body.contains("\r\n");
    let lines: Vec<&str> = body.lines().collect();

    // Parsed once and reused for later whole-block edits; skipped entirely
    // when the patch is just text, the common case.
    let mut opens: Option<BTreeMap<usize, (usize, usize)>> = None;
    let mut placed: Vec<Placed> = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let anchor = edit.anchor.text();
        if anchor.is_empty() {
            return Err(Refusal::EmptyAnchor {
                path: path.to_string(),
                index,
            });
        }
        if !matches!(edit.anchor, Anchor::Replace(_)) && edit.new.is_empty() {
            return Err(Refusal::EmptyInsert {
                path: path.to_string(),
                index,
            });
        }

        if edit.whole_block {
            let opens = opens.get_or_insert_with(|| crate::blocks::by_row(path, body));
            let (span, whole) = block(path, opens, body, &lines, anchor, index)?;
            let text = match edit.anchor {
                Anchor::Replace(_) => splice(&whole, &edit.new, ending(crlf)),
                _ => translate(&edit.new, crlf),
            };
            placed.push(Placed {
                index,
                span: landing(&edit.anchor, span),
                text,
            });
            continue;
        }

        let found = find(body, anchor, crlf);
        if found.is_empty() {
            let seq: Vec<&str> = anchor.lines().collect();
            let (line, text) = nearest(&lines, &seq, 1, lines.len().max(1));
            return Err(Refusal::NoMatch {
                path: path.to_string(),
                index,
                line,
                text: crop(&text, 80),
            });
        }
        if found.len() > 1 && !edit.replace_all {
            return Err(Refusal::Ambiguous {
                path: path.to_string(),
                index,
                n: found.len(),
                detail: candidates(path, body, &lines, &found),
            });
        }
        let text = translate(&edit.new, crlf);
        for span in found {
            placed.push(Placed {
                index,
                span: landing(&edit.anchor, span),
                text: text.clone(),
            });
        }
    }

    placed.sort_by_key(|p| (p.span.start, p.span.end));
    for pair in placed.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        // An insertion has no byte of its own to order by: two on one byte, or
        // one inside a region the other replaces, leave the order a guess.
        if (point(a) || point(b)) && (a.span.start == b.span.start || a.span.end > b.span.start) {
            return Err(Refusal::InsertInside {
                path: path.to_string(),
                a: a.index,
                b: b.index,
            });
        }
        if a.span.end > b.span.start {
            return Err(Refusal::Overlap {
                path: path.to_string(),
                a: a.index,
                b: b.index,
            });
        }
    }

    let mut out = String::with_capacity(body.len());
    let mut cursor = 0usize;
    let mut line = 1usize;
    let mut landed: Vec<Landed> = Vec::new();
    let mut left_blank: Vec<usize> = Vec::new();
    for p in &placed {
        // An emptied line that kept its break leaves a blank row where the
        // model may have meant to take the row with it.
        if p.text.is_empty()
            && !point(p)
            && !body[p.span.start..p.span.end].ends_with('\n')
            && starts_a_break(&body[p.span.end..])
            && !left_blank.contains(&p.index)
        {
            left_blank.push(p.index);
        }
        let gap = &body[cursor..p.span.start];
        out.push_str(gap);
        line += breaks(gap);
        let start = line;
        out.push_str(&p.text);
        line += breaks(&p.text);
        let end = if p.text.is_empty() {
            start.saturating_sub(1)
        } else if p.text.ends_with('\n') {
            line - 1
        } else {
            line
        };
        let took_at = line_of(body, p.span.start);
        let took = if point(p) {
            Vec::new()
        } else {
            let last = line_of(body, p.span.end - 1);
            body.lines()
                .skip(took_at - 1)
                .take(last - took_at + 1)
                .map(str::to_string)
                .collect()
        };
        landed.push(Landed {
            start,
            end,
            took,
            took_at,
        });
        cursor = p.span.end;
    }
    out.push_str(&body[cursor..]);

    let result = format!("{bom}{out}");
    if result == content {
        return Err(Refusal::Unchanged {
            path: path.to_string(),
        });
    }
    Ok(Applied {
        content: result,
        landed,
        left_blank,
    })
}

/// One resolved landing: which edit, where in the original, and the bytes that
/// go there.
struct Placed {
    index: usize,
    span: Range<usize>,
    text: String,
}

/// Where an edit lands once its anchor has matched: the match itself for a
/// replacement, a point at one end of it for the two inserts.
fn landing(anchor: &Anchor, span: Range<usize>) -> Range<usize> {
    match anchor {
        Anchor::Replace(_) => span,
        Anchor::After(_) => span.end..span.end,
        Anchor::Before(_) => span.start..span.start,
    }
}

fn point(p: &Placed) -> bool {
    p.span.start == p.span.end
}

pub(super) fn split_bom(content: &str) -> (&str, &str) {
    match content.strip_prefix('\u{FEFF}') {
        Some(rest) => ("\u{FEFF}", rest),
        None => ("", content),
    }
}

// Every occurrence of the anchor, in order, tried as written first: the
// spellings a view prints and a CRLF file writes are retries, not rewrites.
fn find(body: &str, anchor: &str, crlf: bool) -> Vec<Range<usize>> {
    let mut spellings = vec![anchor.to_string()];
    if let Some(rest) = without_address(anchor)
        && rest != anchor
    {
        spellings.push(rest.to_string());
    }
    if crlf {
        for spelling in spellings.clone() {
            if spelling.contains('\n') {
                spellings.push(translate(&spelling, true));
            }
        }
    }
    for spelling in &spellings {
        let hits = spans(body, spelling);
        if !hits.is_empty() {
            return hits;
        }
    }
    Vec::new()
}

fn spans(body: &str, needle: &str) -> Vec<Range<usize>> {
    body.match_indices(needle)
        .map(|(at, _)| at..at + needle.len())
        .collect()
}

fn ending(crlf: bool) -> &'static str {
    if crlf { "\r\n" } else { "\n" }
}

/// Whether the text opens with a line break, either spelling: a CRLF file
/// puts a `\r` between the row that was emptied and the break it kept.
fn starts_a_break(s: &str) -> bool {
    s.starts_with('\n') || s.starts_with("\r\n")
}

/// The model writes `\n`; a file that ends its lines `\r\n` gets its own
/// spelling back, so one edit never mixes endings inside a file.
fn translate(text: &str, crlf: bool) -> String {
    if !crlf {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut after_cr = false;
    for c in text.chars() {
        if c == '\n' && !after_cr {
            out.push_str("\r\n");
        } else {
            out.push(c);
        }
        after_cr = c == '\r';
    }
    out
}

/// Keeps the region's line ending and trailing-newline state; empty `new`
/// removes the lines and their break entirely.
fn splice(region: &str, new: &str, term: &str) -> String {
    let kept = region.ends_with('\n');
    let normalized = new.replace("\r\n", "\n");
    let mut parts: Vec<&str> = if normalized.is_empty() {
        Vec::new()
    } else {
        normalized.split('\n').collect()
    };
    if normalized.ends_with('\n') {
        parts.pop();
    }
    if parts.is_empty() {
        return String::new();
    }
    let mut out = parts.join(term);
    if kept {
        out.push_str(term);
    }
    out
}

/// The block an anchor names, as its bytes. Matched as a prefix of the
/// opening line, after stripping any `120-145:` line-number prefix.
fn block(
    path: &str,
    opens: &BTreeMap<usize, (usize, usize)>,
    body: &str,
    lines: &[&str],
    anchor: &str,
    index: usize,
) -> Result<(Range<usize>, String), Refusal> {
    // The view's address strips indentation before matching the trimmed line;
    // the row it names disambiguates blocks that open on the same text.
    let addressed = address(anchor).or_else(|| address(anchor.trim_start()));
    let (needle, named) = match addressed.filter(|(_, rest)| !rest.trim().is_empty()) {
        Some((row, rest)) => (rest.trim(), Some(row)),
        None => (anchor.trim(), None),
    };
    // One hit per block, not per row: every row an annotation spans names the
    // same block, so multiple matching rows count as one candidate.
    let mut seen: Vec<(usize, usize)> = Vec::new();
    let mut hits: Vec<usize> = Vec::new();
    for row in opens.keys().copied() {
        if named.is_some_and(|at| at != row) {
            continue;
        }
        if !lines
            .get(row - 1)
            .is_some_and(|l| l.trim().starts_with(needle))
        {
            continue;
        }
        let span = opens[&row];
        if !seen.contains(&span) {
            seen.push(span);
            hits.push(row);
        }
    }
    let at = match hits.len() {
        1 => hits[0],
        0 => {
            return Err(Refusal::NoBlock {
                path: path.to_string(),
                index,
                text: needle.to_string(),
                available: openings(lines, opens),
            });
        }
        n => {
            return Err(Refusal::AmbiguousBlock {
                path: path.to_string(),
                index,
                text: needle.to_string(),
                n,
            });
        }
    };
    // Every row a hit is drawn from is a key of `opens`, so the map holds it.
    let (start, end) = opens[&at];

    // The blank lines a block is followed by separate it from the next one:
    // they belong to the gap, not to what the anchor named.
    let mut last = end.min(lines.len());
    while last > start && lines.get(last - 1).is_some_and(|l| l.trim().is_empty()) {
        last -= 1;
    }
    let starts = line_starts(body);
    let from = starts[start - 1];
    let to = starts.get(last).copied().unwrap_or(body.len());
    Ok((from..to, body[from..to].to_string()))
}

// Lists what the file does open, so the fix is copying one back into the
// anchor. One line per block, keyed by its actual opening row.
fn openings(lines: &[&str], opens: &BTreeMap<usize, (usize, usize)>) -> String {
    let mut starts: Vec<usize> = opens.values().map(|(start, _)| *start).collect();
    starts.sort_unstable();
    starts.dedup();
    let mut rows: Vec<&str> = Vec::new();
    for n in starts {
        if let Some(text) = lines.get(n - 1).map(|l| l.trim())
            && !rows.contains(&text)
        {
            rows.push(text);
        }
    }
    if rows.is_empty() {
        return String::new();
    }
    let shown: Vec<String> = rows
        .iter()
        .take(5)
        .map(|r| format!("`{}`", crop(r, 60)))
        .collect();
    let rest = rows.len().saturating_sub(shown.len());
    if rest > 0 {
        format!(" — the file opens: {}, and {rest} more", shown.join(", "))
    } else {
        format!(" — the file opens: {}", shown.join(", "))
    }
}

/// Splits the `120-145:` prefix a view prints from a line: the row it names,
/// and the text after it.
fn address(text: &str) -> Option<(usize, &str)> {
    let (head, rest) = text.split_once(':')?;
    let mut parts = head.split('-');
    let first = parts.next()?;
    let digit = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    if !digit(first) || !parts.all(digit) {
        return None;
    }
    Some((first.parse().ok()?, rest))
}

/// The anchor with that address taken off, when something is left to match:
/// an address on its own names no text, and nothing matches everywhere.
fn without_address(text: &str) -> Option<&str> {
    let (_, rest) = address(text).or_else(|| address(text.trim_start()))?;
    (!rest.trim().is_empty()).then_some(rest)
}

/// Where each line starts, so `starts[i]` is the byte after line `i`'s break:
/// the end of line `i` including its terminator.
fn line_starts(body: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (at, b) in body.bytes().enumerate() {
        if b == b'\n' {
            starts.push(at + 1);
        }
    }
    starts
}

fn breaks(s: &str) -> usize {
    s.bytes().filter(|b| *b == b'\n').count()
}

/// The 1-based line a byte offset sits on.
fn line_of(body: &str, at: usize) -> usize {
    body.as_bytes()[..at.min(body.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

// Why the anchor was refused, in the words the model needs: where each
// candidate is, what block holds it, and what precedes it.
fn candidates(path: &str, body: &str, lines: &[&str], hits: &[Range<usize>]) -> String {
    // Spelled out for the first few and counted for the rest: an anchor that
    // names a thousand rows is answered with a prefix, not a thousand lines.
    const SHOWN: usize = 8;
    let extents = crate::blocks::spans(path, body);
    let mut out: Vec<String> = hits
        .iter()
        .take(SHOWN)
        .map(|r| {
            let at = line_of(body, r.start);
            let held = covering(at, lines, &extents)
                .map_or_else(String::new, |b| format!(" in `{}`", crop(&b, 60)));
            let before = match at.checked_sub(2).and_then(|i| lines.get(i)) {
                Some(prev) => format!(", preceded by `{}`", crop(prev, 60)),
                None => String::new(),
            };
            format!("  line {at}{held}{before}")
        })
        .collect();
    if let Some(rest) = hits.len().checked_sub(SHOWN).filter(|n| *n > 0) {
        out.push(format!("  … and {rest} more"));
    }
    out.join("\n")
}

// The block a line sits in, innermost first: what a refusal names when it has
// to say where a candidate is.
fn covering(at: usize, lines: &[&str], extents: &[(usize, usize)]) -> Option<String> {
    extents
        .iter()
        .filter(|(s, e)| *s <= at && at <= *e)
        .min_by_key(|(s, e)| e - s)
        .map(|(s, _)| lines.get(s - 1).unwrap_or(&"").trim().to_string())
}

// The closest real line, so a refusal points at one: exact match would have
// hit, so this is char overlap against the anchor's own lines.
fn nearest(lines: &[&str], seq: &[&str], lo: usize, hi: usize) -> (usize, String) {
    let mut best = (lo, lines.get(lo - 1).copied().unwrap_or("").to_string());
    let mut best_score = -1.0f64;
    for n in lo..=hi {
        let row = lines.get(n - 1).copied().unwrap_or("");
        let score = seq.iter().map(|s| sim(row, s)).fold(-1.0f64, f64::max);
        if score > best_score {
            best_score = score;
            best = (n, row.to_string());
        }
    }
    best
}

// Counted common characters over the longer of the two lines.
fn sim(a: &str, b: &str) -> f64 {
    let mut ca: Vec<char> = a.chars().collect();
    let mut cb: Vec<char> = b.chars().collect();
    ca.sort_unstable();
    cb.sort_unstable();
    let (mut i, mut j, mut common) = (0usize, 0usize, 0usize);
    while i < ca.len() && j < cb.len() {
        match ca[i].cmp(&cb[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                common += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let denom = ca.len().max(cb.len()).max(1);
    common as f64 / denom as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replace(old: &str, new: &str) -> Edit {
        Edit {
            anchor: Anchor::Replace(old.into()),
            new: new.into(),
            replace_all: false,
            whole_block: false,
        }
    }

    #[test]
    fn a_later_edit_does_not_see_an_earlier_one() {
        // `two` exists only once the first edit lands, so a call matched
        // incrementally would take it; this one must not.
        let edits = [replace("one", "two"), replace("two", "three")];
        let err = apply("a.rs", "one\n", &edits).unwrap_err();
        assert!(matches!(err, Refusal::NoMatch { index: 1, .. }), "{err}");
    }

    #[test]
    fn two_replacements_over_the_same_text_are_an_overlap() {
        let edits = [replace("x", "1"), replace("x", "2")];
        let err = apply("a.rs", "x\n", &edits).unwrap_err();
        assert!(matches!(err, Refusal::Overlap { .. }), "{err}");
    }
}
