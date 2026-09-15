//! Byte-anchored edits: the surface the edit tool speaks.
//!
//! An anchor is literal text found in the file, a landing is a byte range, and
//! nothing is written unless every edit resolves against the original content.
//! Lines matter in two places only: a block named by its first line, and the
//! line numbers the echo reports.

use std::ops::Range;

use crate::Blocks;

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

impl Landed {
    /// How many lines this put in the file.
    pub fn gave(&self) -> usize {
        (self.end + 1).saturating_sub(self.start)
    }
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
    /// stands where it was: `new_string: ""` on an anchor that ends at the end
    /// of a line. The tool says so rather than guessing the model's intent.
    pub left_blank: Vec<usize>,
}

impl Refusal {
    /// The stable tag a refusal is logged and grouped under, whatever its
    /// prose says.
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

    /// Whether the call itself is spelt wrong, as opposed to not fitting the
    /// file: the two are different lessons and the loop counts them apart.
    pub fn is_shape(&self) -> bool {
        matches!(
            self,
            Refusal::NoEdits | Refusal::EmptyAnchor { .. } | Refusal::EmptyInsert { .. }
        )
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
/// `blocks` resolves a `whole_block` anchor; `NoBlocks` refuses them all.
pub fn apply(
    path: &str,
    content: &str,
    edits: &[Edit],
    blocks: &dyn Blocks,
) -> Result<Applied, Refusal> {
    if edits.is_empty() {
        return Err(Refusal::NoEdits);
    }
    // A byte-order mark is invisible to the model, so it is never part of an
    // anchor: match without it and write it back.
    let (bom, body) = split_bom(content);
    let crlf = body.contains("\r\n");
    let lines: Vec<&str> = body.lines().collect();

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
            let (span, whole) = block(path, body, &lines, anchor, blocks, index)?;
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
                detail: candidates(path, body, &lines, &found, blocks),
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

fn split_bom(content: &str) -> (&str, &str) {
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

/// A whole block's replacement text, spliced the way its lines were: the region
/// is whole lines, so the new text takes the region's own line ending and the
/// file's trailing-newline state is left as it was. An empty new text removes
/// the lines and their break, leaving no blank behind.
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

/// The block an anchor names: its bytes, and those bytes whole. The anchor is
/// matched as a prefix of a block-opening line, with the `120-145:` a view
/// prints stripped, since that is what gets copied back into it.
fn block(
    path: &str,
    body: &str,
    lines: &[&str],
    anchor: &str,
    blocks: &dyn Blocks,
    index: usize,
) -> Result<(Range<usize>, String), Refusal> {
    // The address a view prints leaves the line's own indentation on the
    // text, which a prefix match against a trimmed opening line does not want.
    let needle = without_address(anchor)
        .unwrap_or_else(|| anchor.trim())
        .trim();
    let opens = blocks.openings(path, body);
    let hits: Vec<usize> = opens
        .iter()
        .copied()
        .filter(|n| {
            lines
                .get(n - 1)
                .is_some_and(|l| l.trim().starts_with(needle))
        })
        .collect();
    let at = match hits.len() {
        1 => hits[0],
        0 => {
            return Err(Refusal::NoBlock {
                path: path.to_string(),
                index,
                text: needle.to_string(),
                available: openings(lines, &opens),
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
    let (start, end) = blocks
        .extent_of(path, body, at)
        .ok_or_else(|| Refusal::NoBlock {
            path: path.to_string(),
            index,
            text: needle.to_string(),
            available: " — the line it names opens no resolvable block".to_string(),
        })?;

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

// The refusal hands over what the file does open, since the fix is copying one
// of these back into the anchor.
fn openings(lines: &[&str], opens: &[usize]) -> String {
    let mut rows: Vec<&str> = opens
        .iter()
        .filter_map(|n| lines.get(n - 1))
        .map(|l| l.trim())
        .collect();
    rows.dedup();
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

/// The `120-145:` a view prints in front of a line, split off — what a model
/// copies back along with the line itself.
fn address(text: &str) -> Option<&str> {
    let (head, rest) = text.split_once(':')?;
    let digits = head
        .split('-')
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    digits.then_some(rest)
}

/// The anchor with that address taken off, when something is left to match:
/// an address on its own names no text, and nothing matches everywhere.
fn without_address(text: &str) -> Option<&str> {
    let rest = address(text).or_else(|| address(text.trim_start()))?;
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
fn candidates(
    path: &str,
    body: &str,
    lines: &[&str],
    hits: &[Range<usize>],
    blocks: &dyn Blocks,
) -> String {
    // Spelled out for the first few and counted for the rest: an anchor that
    // names a thousand rows is answered with a prefix, not a thousand lines.
    const SHOWN: usize = 8;
    let extents = blocks.extents(path, body);
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

fn crop(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let mut t: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        t.push('…');
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NoBlocks;

    // A parser stand-in: a line starting `fn ` covers up to the next one, so
    // a fixture can leave a blank inside the first block, where a head sits.
    struct Sections;

    impl Blocks for Sections {
        fn extent_of(&self, _path: &str, content: &str, line: usize) -> Option<(usize, usize)> {
            let lines: Vec<&str> = content.lines().collect();
            lines
                .get(line - 1)?
                .trim_start()
                .starts_with("fn ")
                .then(|| {
                    let next = lines
                        .iter()
                        .enumerate()
                        .skip(line)
                        .find(|(_, l)| l.trim_start().starts_with("fn "))
                        .map_or(lines.len() + 1, |(i, _)| i + 1);
                    (line, next - 1)
                })
        }

        fn openings(&self, _path: &str, content: &str) -> Vec<usize> {
            content
                .lines()
                .enumerate()
                .filter(|(_, l)| l.trim_start().starts_with("fn "))
                .map(|(i, _)| i + 1)
                .collect()
        }
    }

    fn edit(anchor: Anchor, new: &str) -> Edit {
        Edit {
            anchor,
            new: new.into(),
            replace_all: false,
            whole_block: false,
        }
    }

    fn replace(old: &str, new: &str) -> Edit {
        edit(Anchor::Replace(old.into()), new)
    }

    fn after(anchor: &str, new: &str) -> Edit {
        edit(Anchor::After(anchor.into()), new)
    }

    fn before(anchor: &str, new: &str) -> Edit {
        edit(Anchor::Before(anchor.into()), new)
    }

    fn ok(content: &str, edits: &[Edit]) -> Applied {
        apply("a.rs", content, edits, &NoBlocks).expect("applies")
    }

    #[test]
    fn a_fragment_inside_a_line_is_replaced() {
        let out = ok(
            "let mut x = 1;\nlet y = 2;\n",
            &[replace("let mut x", "let x")],
        );
        assert_eq!(out.content, "let x = 1;\nlet y = 2;\n");
        assert_eq!(out.landed[0].took_at, 1);
        assert_eq!(out.landed[0].took, vec!["let mut x = 1;".to_string()]);
    }

    #[test]
    fn a_missing_anchor_points_at_the_nearest_line() {
        let err = apply(
            "a.rs",
            "fn one() {\n}\n",
            &[replace("fn two() {", "x")],
            &NoBlocks,
        )
        .unwrap_err();
        assert!(matches!(err, Refusal::NoMatch { line: 1, .. }), "{err}");
    }

    #[test]
    fn an_anchor_naming_two_places_is_refused_with_both() {
        let err = apply(
            "a.rs",
            "let a = 1;\nlet a = 2;\n",
            &[replace("let a = ", "let b = ")],
            &NoBlocks,
        )
        .unwrap_err();
        match err {
            Refusal::Ambiguous { n, detail, .. } => {
                assert_eq!(n, 2);
                assert!(detail.contains("line 1"), "{detail}");
                assert!(detail.contains("line 2"), "{detail}");
            }
            other => panic!("{other}"),
        }
    }

    #[test]
    fn replace_all_takes_every_occurrence() {
        let mut edit = replace("let a = ", "let b = ");
        edit.replace_all = true;
        let out = ok("let a = 1;\nlet a = 2;\n", &[edit]);
        assert_eq!(out.content, "let b = 1;\nlet b = 2;\n");
        assert_eq!(out.landed.len(), 2);
    }

    #[test]
    fn an_insert_after_a_line_lands_at_the_next_line_start() {
        let out = ok("a;\nb;\n", &[after("a;\n", "new;\n")]);
        assert_eq!(out.content, "a;\nnew;\nb;\n");
        // The insert is named by the row it lands on: line 2 in the original,
        // line 2 in the result.
        assert_eq!((out.landed[0].took_at, out.landed[0].start), (2, 2));
        assert_eq!(out.landed[0].gave(), 1);
        assert!(out.landed[0].took.is_empty(), "an insert displaces nothing");
    }

    #[test]
    fn an_insert_inside_a_line_stays_on_it() {
        let out = ok("let x = 1;\n", &[after("let x", " /* here */")]);
        assert_eq!(out.content, "let x /* here */ = 1;\n");
    }

    #[test]
    fn an_insert_before_lands_before_the_match() {
        let out = ok("fn f() {}\n", &[before("fn f", "/// doc\n")]);
        assert_eq!(out.content, "/// doc\nfn f() {}\n");
    }

    #[test]
    fn a_crlf_file_answers_an_lf_anchor_and_keeps_its_endings() {
        let out = ok("a;\r\nb;\r\n", &[replace("a;\n", "a2;\n")]);
        assert_eq!(out.content, "a2;\r\nb;\r\n");
    }

    #[test]
    fn an_insert_into_a_crlf_file_lands_with_its_endings() {
        let out = ok("a;\r\n", &[after("a;\r\n", "new;\n")]);
        assert_eq!(out.content, "a;\r\nnew;\r\n");
    }

    #[test]
    fn a_byte_order_mark_is_matched_through_and_kept() {
        let out = ok("\u{FEFF}# Title\ntext\n", &[replace("# Title", "# T")]);
        assert_eq!(out.content, "\u{FEFF}# T\ntext\n");
    }

    #[test]
    fn a_whole_block_is_replaced_from_its_first_line() {
        let mut edit = replace("fn f()", "fn f() {\n    c;\n}\n");
        edit.whole_block = true;
        let out = apply(
            "a.rs",
            "fn f() {\n    a;\n}\n\nfn g() {\n    b;\n}\n",
            &[edit],
            &Sections,
        )
        .expect("applies");
        assert_eq!(out.content, "fn f() {\n    c;\n}\n\nfn g() {\n    b;\n}\n");
        assert_eq!(out.landed[0].took, vec!["fn f() {", "    a;", "}"]);
    }

    #[test]
    fn a_block_that_ends_in_a_blank_line_gives_the_blank_back() {
        let mut edit = replace("fn f()", "fn f() {\n}\n");
        edit.whole_block = true;
        let out = apply(
            "a.rs",
            "fn f() {\n    a;\n}\n\nfn g() {\n}\n",
            &[edit],
            &Sections,
        )
        .expect("applies");
        assert_eq!(out.content, "fn f() {\n}\n\nfn g() {\n}\n");
    }

    #[test]
    fn deleting_a_whole_block_leaves_no_blank_line() {
        let mut edit = replace("fn f()", "");
        edit.whole_block = true;
        let out = apply(
            "a.rs",
            "fn f() {\n    a;\n}\nfn g() {\n}\n",
            &[edit],
            &Sections,
        )
        .expect("applies");
        assert_eq!(out.content, "fn g() {\n}\n");
    }

    #[test]
    fn an_insert_after_a_block_lands_past_its_last_line() {
        let mut edit = after("fn f()", "fn g() {\n}\n");
        edit.whole_block = true;
        let out = apply("a.rs", "fn f() {\n    a;\n}\n", &[edit], &Sections).expect("applies");
        assert_eq!(out.content, "fn f() {\n    a;\n}\nfn g() {\n}\n");
    }

    #[test]
    fn a_line_pasted_from_a_view_drops_its_address() {
        // `read` and `grep` print `2:` in front of a line, and that is what
        // gets copied back into an anchor.
        let out = ok(
            "a;\n// TODO: rename\n",
            &[replace("2:// TODO: rename", "// renamed")],
        );
        assert_eq!(out.content, "a;\n// renamed\n");
    }

    #[test]
    fn an_address_that_is_really_in_the_file_still_matches() {
        // The retry is a fallback, not a rewrite: text that really says `2:`
        // is matched as written.
        let out = ok("2: real\n", &[replace("2: real", "real")]);
        assert_eq!(out.content, "real\n");
    }

    #[test]
    fn an_anchor_pasted_from_a_view_keeps_its_address_out_of_the_match() {
        let mut edit = replace("1-3:fn f() {", "fn f() {\n}\n");
        edit.whole_block = true;
        let out = apply("a.rs", "fn f() {\n    a;\n}\n", &[edit], &Sections).expect("applies");
        assert_eq!(out.content, "fn f() {\n}\n");
    }

    #[test]
    fn a_whole_block_without_a_parser_is_refused() {
        let mut edit = replace("fn f()", "x");
        edit.whole_block = true;
        let err = apply("a.rs", "fn f() {\n}\n", &[edit], &NoBlocks).unwrap_err();
        assert!(matches!(err, Refusal::NoBlock { .. }), "{err}");
    }

    #[test]
    fn an_empty_anchor_is_refused() {
        let err = apply("a.rs", "x\n", &[replace("", "y")], &NoBlocks).unwrap_err();
        assert!(
            matches!(err, Refusal::EmptyAnchor { index: 0, .. }),
            "{err}"
        );
    }

    #[test]
    fn inserting_nothing_is_refused() {
        let err = apply("a.rs", "x\n", &[after("x", "")], &NoBlocks).unwrap_err();
        assert!(
            matches!(err, Refusal::EmptyInsert { index: 0, .. }),
            "{err}"
        );
    }

    #[test]
    fn an_insert_inside_a_replaced_region_is_refused() {
        let edits = [
            replace("let a = 1;\nlet b = 2;\n", "let a = 3;\n"),
            after("let a", " // x"),
        ];
        let err = apply("a.rs", "let a = 1;\nlet b = 2;\n", &edits, &NoBlocks).unwrap_err();
        assert!(matches!(err, Refusal::InsertInside { .. }), "{err}");
    }

    #[test]
    fn overlapping_replacements_are_refused() {
        let edits = [
            replace("let a = 1;\nlet b", "X"),
            replace("let b = 2;", "Y"),
        ];
        let err = apply("a.rs", "let a = 1;\nlet b = 2;\n", &edits, &NoBlocks).unwrap_err();
        assert!(matches!(err, Refusal::Overlap { .. }), "{err}");
    }

    #[test]
    fn a_later_edit_does_not_see_an_earlier_one() {
        // `two` exists only once the first edit lands, so a call matched
        // incrementally would take it; this one must not.
        let edits = [replace("one", "two"), replace("two", "three")];
        let err = apply("a.rs", "one\n", &edits, &NoBlocks).unwrap_err();
        assert!(matches!(err, Refusal::NoMatch { index: 1, .. }), "{err}");
    }

    #[test]
    fn an_edit_that_changes_nothing_is_refused() {
        let err = apply("a.rs", "x\n", &[replace("x", "x")], &NoBlocks).unwrap_err();
        assert!(matches!(err, Refusal::Unchanged { .. }), "{err}");
    }

    #[test]
    fn a_call_with_no_edits_is_refused() {
        assert_eq!(
            apply("a.rs", "x\n", &[], &NoBlocks).unwrap_err(),
            Refusal::NoEdits
        );
    }

    #[test]
    fn an_anchor_that_is_only_an_address_matches_nothing() {
        // A view prints a blank row as `3:`, and that is what gets copied
        // back: with the address taken off, nothing is left to match.
        let src = "a;\n\nb;\n";
        for anchor in ["2:", "2:\n", "  2:"] {
            let err = apply("a.rs", src, &[replace(anchor, "c;")], &NoBlocks).unwrap_err();
            assert!(matches!(err, Refusal::NoMatch { .. }), "`{anchor}`: {err}");
        }
    }

    #[test]
    fn a_whole_block_anchor_of_only_an_address_names_no_block() {
        let mut edit = replace("2:", "x");
        edit.whole_block = true;
        let err = apply("a.rs", "fn f() {\n}\n", &[edit], &Sections).unwrap_err();
        assert!(matches!(err, Refusal::NoBlock { .. }), "{err}");
    }

    #[test]
    fn a_refusal_over_many_candidates_names_a_few_and_counts_the_rest() {
        let src: String = (1..=20).map(|i| format!("a(); // {i}\n")).collect();
        let err = apply("a.rs", &src, &[replace("a();", "b();")], &NoBlocks).unwrap_err();
        match err {
            Refusal::Ambiguous { n, detail, .. } => {
                assert_eq!(n, 20);
                assert_eq!(detail.lines().count(), 9, "{detail}");
                assert!(detail.ends_with("… and 12 more"), "{detail}");
            }
            other => panic!("{other}"),
        }
    }

    #[test]
    fn two_replacements_over_the_same_text_are_an_overlap() {
        let edits = [replace("x", "1"), replace("x", "2")];
        let err = apply("a.rs", "x\n", &edits, &NoBlocks).unwrap_err();
        assert!(matches!(err, Refusal::Overlap { .. }), "{err}");
    }

    #[test]
    fn emptying_a_row_in_a_crlf_file_says_the_break_is_left() {
        let out = ok("a;\r\nb;\r\n", &[replace("b;", "")]);
        assert_eq!(out.content, "a;\r\n\r\n");
        assert_eq!(out.left_blank, vec![0]);
    }
    #[test]
    fn replacing_non_ascii_anchor_does_not_panic() {
        let out = ok("hello 小可爱\n", &[replace("小可爱", "猫")]);
        assert_eq!(out.content, "hello 猫\n");
        assert_eq!(out.landed[0].took, vec!["hello 小可爱".to_string()]);
    }
}
