use std::io::{IsTerminal, Write};

use agent::Event;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const CODE: &str = "\x1b[33m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// Whether the surface being written to can carry colour.
#[derive(Debug, Clone, Copy)]
pub struct Paint {
    pub color: bool,
}

impl Paint {
    pub fn on(&self, code: &str, body: &str) -> String {
        if self.color {
            format!("{code}{body}{RESET}")
        } else {
            body.to_string()
        }
    }
}

/// Terminal styling for the markdown a model writes, decided one line at a
/// time.
///
/// Forward-only, because a line the terminal has printed cannot be restyled:
/// what a row looks like is settled when it ends, out of what came before it.
/// That rules out anything needing the whole document — a table's column
/// widths, a reflowed code block — and leaves what a coding agent actually
/// emits.
#[derive(Debug, Default, Clone, Copy)]
pub struct Markdown {
    /// Inside a ``` block, where nothing is markup and everything is code.
    fenced: bool,
}

impl Markdown {
    /// The line as the terminal should show it.
    ///
    /// Takes `&self`, not `&mut`: the row still being written is styled again
    /// on every frame, and only a line that has ended may decide what the one
    /// after it means.
    pub fn line(&self, text: &str, p: Paint) -> String {
        if !p.color {
            return text.to_string();
        }
        if fence(text) {
            return p.on(DIM, text);
        }
        if self.fenced {
            // A gutter rather than a colour: code has to stay the most legible
            // thing on the screen, and thirty yellow rows is the opposite.
            return format!("{}{text}", p.on(DIM, "│ "));
        }
        let body = text.trim_start();
        let pad = &text[..text.len() - body.len()];
        if body.starts_with("> ") {
            // Whole-line, no spans inside: a quote is an aside, and dimming it
            // is the whole of what it needs said.
            return p.on(DIM, text);
        }
        if let Some(at) = heading(body) {
            let marker = p.on(DIM, &body[..at]);
            return format!("{pad}{marker}{BOLD}{}{RESET}", spans(&body[at..], BOLD, 0));
        }
        match bullet(body) {
            Some(at) => format!(
                "{pad}{}{}",
                p.on(DIM, &body[..at]),
                spans(&body[at..], "", 0)
            ),
            None => format!("{pad}{}", spans(body, "", 0)),
        }
    }

    /// A line has ended. A fence is the only thing in it that changes what the
    /// line after it means.
    pub fn advance(&mut self, text: &str) {
        if fence(text) {
            self.fenced = !self.fenced;
        }
    }

    /// A new run starts outside any block, whatever the last one left open.
    pub fn reset(&mut self) {
        self.fenced = false;
    }
}

fn fence(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// Where a heading's `#`s and their space end, if the line is one.
///
/// The space is what makes it a heading rather than a line that merely opens
/// with a hash — which, in a tree full of attributes and shell comments, is
/// most of them.
fn heading(body: &str) -> Option<usize> {
    let hashes = body.len() - body.trim_start_matches('#').len();
    ((1..=6).contains(&hashes) && body[hashes..].starts_with(' ')).then_some(hashes + 1)
}

/// Where a list item's marker ends, if the line opens with one.
fn bullet(body: &str) -> Option<usize> {
    if body.starts_with("- ") || body.starts_with("* ") || body.starts_with("+ ") {
        return Some(2);
    }
    let digits = body.len() - body.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    (digits > 0 && body[digits..].starts_with(". ")).then_some(digits + 2)
}

/// How deep emphasis may hold more emphasis.
///
/// Real markdown nests one level, at most two. The bound is not about taste: a
/// span recurses on its own body, so a long enough line of `**`*` would put the
/// stack in the hands of whatever the model wrote.
const NESTING: u8 = 3;

/// The inline spans of one line: code, bold, italic.
///
/// `under` is whatever styling is already open around `text`. A span closes
/// with a reset — there is no escape for "bold off" that leaves the rest
/// standing — so it has to re-open what it interrupted, or a code span inside
/// bold ends the bold at the backtick and the sentence after it goes plain.
fn spans(text: &str, under: &str, depth: u8) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let Some(at) = rest.find(['`', '*']) else {
            out.push_str(rest);
            break;
        };
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        match span(from) {
            Some((code, body, tail)) => {
                // Code is literal all the way down; emphasis can hold more.
                let inner = if code == CODE || depth == NESTING {
                    body.to_string()
                } else {
                    spans(body, &format!("{under}{code}"), depth + 1)
                };
                out.push_str(&format!("{code}{inner}{RESET}{under}"));
                rest = tail;
            }
            // An opener with no closer is text: the line is still arriving, or
            // the character meant itself.
            None => {
                let mut chars = from.chars();
                out.push(chars.next().unwrap_or_default());
                rest = chars.as_str();
            }
        }
    }
    out
}

/// One span at the head of `from`: its code, its text, and what follows it.
///
/// `_` is not a delimiter here. It is the word separator of every identifier in
/// the tree, and a rule that italicises the middle of `saturating_sub` is worse
/// than no italics at all.
fn span(from: &str) -> Option<(&'static str, &str, &str)> {
    for (mark, code) in [("**", BOLD), ("`", CODE), ("*", ITALIC)] {
        let Some(rest) = from.strip_prefix(mark) else {
            continue;
        };
        let Some(end) = rest.find(mark) else {
            continue;
        };
        let body = &rest[..end];
        // Flanking, for the emphasis marks only: without it `2 * 3 * 4` reads
        // as an italic 3. A backtick means code wherever it lands.
        let loose = mark != "`"
            && (body.starts_with(char::is_whitespace) || body.ends_with(char::is_whitespace));
        if body.is_empty() || loose {
            continue;
        }
        return Some((code, body, &rest[end + mark.len()..]));
    }
    None
}

/// What a run has cost, in the one wording every place that says it uses.
///
/// A tilde marks the part we counted ourselves, and only that part. The cost
/// carries one whenever any part did, because a number derived from a guess is
/// not a bill.
pub fn spent(usage: &brain::stream::Usage, cost: f64, estimated: agent::Estimated) -> String {
    let mark = agent::Estimated::mark;
    format!(
        "{}{} in / {}{} out · {}{} cached{}",
        mark(estimated.input),
        usage.input,
        mark(estimated.output),
        usage.output,
        mark(estimated.cache_read),
        usage.cache_read,
        // An unpriced model reports no cost rather than $0.
        if cost > 0.0 {
            format!(" · {}${cost:.4}", mark(estimated.any()))
        } else {
            String::new()
        },
    )
}

/// The wording for every event that occupies a whole line.
///
/// Both surfaces call this: a tool call has to read the same in a pipe as in
/// the terminal, and two copies of the wording would drift on the first edit.
/// Events that are a fragment rather than a line — the two deltas — are the
/// caller's to place, and return None.
/// A run's line for one event, and for a tool that offers one, the rows of
/// detail under it.
///
/// Newline-separated, because the caller decides what a row is: the interactive
/// surface repaints a region and has to hand them over one at a time.
pub fn describe(event: &Event, p: Paint, width: usize) -> Option<String> {
    let room = width.saturating_sub(2).max(20);
    Some(match event {
        Event::ToolStart { name, args, .. } => {
            format!("{} {name} {}", p.on(DIM, "→"), p.on(DIM, &summarize(args)))
        }
        Event::ToolEnd {
            name,
            is_error,
            preview,
            ..
        } => {
            let mark = if *is_error {
                p.on(RED, "✗")
            } else {
                p.on(GREEN, "✓")
            };
            let (head, rest) = preview.split_once('\n').unwrap_or((preview, ""));
            let mut out = format!("{mark} {name} {}", p.on(DIM, &clip(head, room)));
            for row in rest.lines() {
                // The mark is the whole of what a diff row means, and colour
                // says it faster than reading the first character does.
                let code = match row.as_bytes().first() {
                    Some(b'+') => GREEN,
                    Some(b'-') => RED,
                    _ => DIM,
                };
                out.push('\n');
                out.push_str(&p.on(code, &format!("  {}", clip(row, room))));
            }
            out
        }
        Event::ToolDenied { name, reason, .. } => {
            format!(
                "{} {name} {}",
                p.on(RED, "✗"),
                p.on(DIM, &clip(reason, room))
            )
        }
        Event::Compacted(r) => p.on(DIM, &compaction_line(r)),
        Event::Retrying {
            attempt,
            delay_ms,
            reason,
        } => p.on(
            DIM,
            &format!("retry {attempt} in {delay_ms}ms · {}", clip(reason, room)),
        ),
        Event::Warning(w) => format!("{} {}", p.on(RED, "!"), p.on(DIM, w)),
        Event::Done {
            turns,
            usage,
            cost,
            estimated,
        } => p.on(
            DIM,
            &format!("{turns} turns · {}", spent(usage, *cost, *estimated)),
        ),
        _ => return None,
    })
}

pub struct Renderer {
    paint: Paint,
    quiet: bool,
    /// A schema was asked for, so stdout belongs to the result alone. Prose the
    /// model produces on the way there is progress, not the answer.
    structured: bool,
    thinking: bool,
    /// Each stream is tracked separately: they share a terminal when both are
    /// a tty, but only the dirty one may be terminated when piped apart.
    out_dirty: bool,
    err_dirty: bool,
}

impl Renderer {
    pub fn new(quiet: bool, structured: bool) -> Self {
        Self {
            paint: Paint {
                color: std::io::stderr().is_terminal(),
            },
            quiet,
            structured,
            thinking: false,
            out_dirty: false,
            err_dirty: false,
        }
    }

    /// Answer text goes to stdout so it pipes; everything else is progress and
    /// goes to stderr.
    pub fn on(&mut self, event: Event) {
        match &event {
            Event::ReasoningDelta(d) if !self.quiet => {
                if !self.thinking {
                    self.settle_out();
                    eprint!("{}", self.paint.on(DIM, "thinking "));
                    self.thinking = true;
                }
                eprint!("{}", self.paint.on(DIM, d));
                self.err_dirty = true;
                let _ = std::io::stderr().flush();
            }
            Event::TextDelta(d) if self.structured => {
                if self.quiet {
                    return;
                }
                self.end_thinking();
                self.settle_out();
                eprint!("{}", self.paint.on(DIM, d));
                self.err_dirty = !d.ends_with('\n');
                let _ = std::io::stderr().flush();
            }
            Event::TextDelta(d) => {
                self.end_thinking();
                self.settle_err();
                print!("{d}");
                self.out_dirty = !d.ends_with('\n');
                let _ = std::io::stdout().flush();
            }
            // Worth seeing even under --quiet: the run did less than it was asked.
            Event::ToolDenied { .. } => {
                self.settle();
                if let Some(line) = describe(&event, self.paint, 100) {
                    eprintln!("{line}");
                }
            }
            _ if self.quiet => {}
            _ => {
                if let Some(line) = describe(&event, self.paint, 100) {
                    self.end_thinking();
                    self.settle();
                    eprintln!("{line}");
                }
            }
        }
    }

    fn end_thinking(&mut self) {
        self.thinking = false;
    }

    /// Terminate the answer stream's partial line. Never called between two
    /// text deltas: they continue one line, they do not each start one.
    fn settle_out(&mut self) {
        if self.out_dirty {
            println!();
            self.out_dirty = false;
        }
    }

    fn settle_err(&mut self) {
        if self.err_dirty {
            eprintln!();
            self.err_dirty = false;
        }
    }

    /// Before a whole-line write, which must start at column zero on both.
    fn settle(&mut self) {
        self.settle_out();
        self.settle_err();
    }

    pub fn finish(&mut self) {
        self.end_thinking();
        self.settle();
    }
}

/// Says what was given up, not just how much. A silent shrink looks like the
/// agent forgetting things for no reason.
fn compaction_line(r: &agent::compact::Report) -> String {
    let mut parts = Vec::new();
    if r.superseded > 0 {
        parts.push(format!("{} superseded", r.superseded));
    }
    if r.uneventful > 0 {
        parts.push(format!("{} uneventful", r.uneventful));
    }
    if r.aged_out > 0 {
        parts.push(format!("{} aged out", r.aged_out));
    }
    if r.dropped > 0 {
        let how = if r.summarized {
            "summarized"
        } else {
            "dropped"
        };
        parts.push(format!("{} messages {how}", r.dropped));
    }
    let detail = if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(", "))
    };
    let warn = if r.still_over {
        " · still over budget"
    } else {
        ""
    };
    format!("compacted {} → {} tokens{detail}{warn}", r.before, r.after)
}

/// One line, cut to `max` columns.
///
/// Columns rather than characters: what overflows a terminal is columns, and a
/// line of Chinese fits half as many characters in the same width. Counting
/// characters let a `grep` pattern or a refusal written in Chinese run to twice
/// the intended width and wrap.
pub fn clip(s: &str, max: usize) -> String {
    let one = s.replace('\n', " ");
    let mut used = 0;
    for (i, c) in one.char_indices() {
        used += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used > max {
            return format!("{}…", one[..i].trim_end());
        }
    }
    one
}

/// The one argument worth showing in a progress line.
pub fn summarize(args: &serde_json::Value) -> String {
    // A patch is many lines; the files it touches are the useful part.
    if let Some(patch) = args.get("patch").and_then(|v| v.as_str()) {
        let files: Vec<&str> = patch
            .lines()
            .filter_map(|l| {
                l.trim()
                    .strip_prefix('[')?
                    .strip_suffix(']')?
                    .rsplit_once('#')
            })
            .map(|(path, _)| path)
            .collect();
        return clip(&files.join(" "), 80);
    }
    // `pattern` before `path`: a grep call carries both, and the pattern is the
    // half that says what the agent was looking for.
    for key in ["pattern", "command", "path", "query"] {
        if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
            return clip(v, 80);
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::{Markdown, Paint, summarize};
    use serde_json::json;

    #[test]
    fn a_patch_summarizes_to_the_files_it_touches() {
        let patch = "[a.rs#A1B2]\nPUT 1.=1:\n+x\n[b.rs#C3D4]\nREM\n";
        assert_eq!(summarize(&json!({ "patch": patch })), "a.rs b.rs");
    }

    #[test]
    fn a_schema_keeps_prose_off_stdout() {
        let mut r = super::Renderer::new(false, true);
        r.on(agent::Event::TextDelta("thinking out loud".into()));
        // stdout carries the result and nothing else, so it pipes into jq.
        assert!(!r.out_dirty, "prose must not reach stdout under a schema");
        assert!(r.err_dirty);
    }

    #[test]
    fn consecutive_text_deltas_stay_on_one_line() {
        let mut r = super::Renderer::new(false, false);
        r.on(agent::Event::TextDelta("There".into()));
        assert!(r.out_dirty, "an unterminated delta leaves the line open");
        r.on(agent::Event::TextDelta("'s a bug".into()));
        // settle_out must not fire between deltas, or every token gets its own line.
        assert!(r.out_dirty);
        r.on(agent::Event::TextDelta("done\n".into()));
        assert!(!r.out_dirty, "a delta ending in a newline closes the line");
    }

    #[test]
    fn other_tools_show_their_leading_argument() {
        assert_eq!(summarize(&json!({ "path": "src/a.rs" })), "src/a.rs");
        assert_eq!(summarize(&json!({ "command": "cargo test" })), "cargo test");
        assert_eq!(summarize(&json!({ "nothing": 1 })), "");
    }

    #[test]
    fn a_compaction_line_names_what_was_given_up() {
        let r = agent::compact::Report {
            before: 130_000,
            after: 48_000,
            superseded: 3,
            uneventful: 1,
            aged_out: 6,
            dropped: 0,
            summarized: false,
            still_over: false,
        };
        assert_eq!(
            super::compaction_line(&r),
            "compacted 130000 → 48000 tokens · 3 superseded, 1 uneventful, 6 aged out"
        );
    }

    #[test]
    fn a_summarized_drop_says_so_rather_than_reading_as_a_loss() {
        let r = agent::compact::Report {
            before: 9,
            after: 5,
            dropped: 4,
            summarized: true,
            ..Default::default()
        };
        assert!(super::compaction_line(&r).contains("4 messages summarized"));
    }

    #[test]
    fn a_compaction_that_did_not_fit_says_so() {
        let r = agent::compact::Report {
            before: 9,
            after: 9,
            still_over: true,
            ..Default::default()
        };
        assert!(super::compaction_line(&r).ends_with("still over budget"));
    }

    /// The styling, with the escapes spelled out so a test reads as what the
    /// terminal receives.
    fn md(text: &str) -> String {
        Markdown::default()
            .line(text, Paint { color: true })
            .replace('\x1b', "^")
    }

    #[test]
    fn emphasis_and_code_are_marked_and_the_delimiters_go() {
        assert_eq!(md("a **b** c"), "a ^[1mb^[0m c");
        assert_eq!(md("a `b` c"), "a ^[33mb^[0m c");
        assert_eq!(md("a *b* c"), "a ^[3mb^[0m c");
    }

    #[test]
    fn an_identifier_is_not_emphasis() {
        // `_` is the word separator of every identifier in the tree; a rule
        // that italicises the middle of one is worse than no italics at all.
        assert_eq!(md("call saturating_sub twice"), "call saturating_sub twice");
        // And a lone `*` between spaces is arithmetic, not an opener.
        assert_eq!(md("2 * 3 * 4"), "2 * 3 * 4");
    }

    #[test]
    fn an_opener_with_no_closer_is_text() {
        // The line is still arriving, or the character meant itself.
        assert_eq!(md("what **half a"), "what **half a");
        assert_eq!(md("a `b"), "a `b");
    }

    #[test]
    fn a_heading_needs_its_space() {
        assert_eq!(md("## Why"), "^[2m## ^[0m^[1mWhy^[0m");
        // Otherwise every `#[derive]` and every shell comment is a heading.
        assert_eq!(md("#[derive(Debug)]"), "#[derive(Debug)]");
    }

    #[test]
    fn a_span_inside_emphasis_re_opens_what_it_interrupted() {
        // The model writes this constantly. Without the re-open the bold ends
        // at the backtick and everything after it goes plain.
        assert_eq!(
            md("- **`unwrap()`**：取出"),
            "^[2m- ^[0m^[1m^[33munwrap()^[0m^[1m^[0m：取出"
        );
    }

    #[test]
    fn a_bullet_keeps_its_marker_and_styles_the_rest() {
        assert_eq!(md("- a **b**"), "^[2m- ^[0ma ^[1mb^[0m");
        assert_eq!(md("  1. a"), "  ^[2m1. ^[0ma");
    }

    #[test]
    fn nesting_stops_before_the_stack_does() {
        // A span recurses on its own body; without a bound a long enough line
        // of `**`*` would put the stack in the hands of whatever was written.
        let line = "**".to_string() + &"*a*".repeat(4000) + "**";
        assert!(Markdown::default().line(&line, Paint { color: true }).len() > line.len());
    }

    #[test]
    fn a_fence_holds_until_the_next_one() {
        let mut m = Markdown::default();
        let p = Paint { color: true };
        assert!(!m.line("fn f() {}", p).contains('\x1b'), "prose is prose");
        m.advance("```rust");
        // Inside, nothing is markup: a gutter, and the text as written.
        assert_eq!(
            m.line("let a = *b;", p).replace('\x1b', "^"),
            "^[2m│ ^[0mlet a = *b;"
        );
        m.advance("let a = *b;");
        m.advance("```");
        assert_eq!(
            m.line("done **now**", p).replace('\x1b', "^"),
            "done ^[1mnow^[0m"
        );
    }

    #[test]
    fn a_plain_surface_is_left_alone() {
        // Piped output is read by something that does not want escapes.
        let out = Markdown::default().line("a **b** `c`", Paint { color: false });
        assert_eq!(out, "a **b** `c`");
    }

    #[test]
    fn a_search_shows_what_it_looked_for_not_where() {
        let args = json!({ "pattern": "fn tier", "path": "crates/tools/src" });
        assert_eq!(summarize(&args), "fn tier");
    }
}
