//! Drawing: what a painted span is, how a theme's style becomes one, and the
//! renderer that writes a one-shot run's rows.
//!
//! What a config names is below this (`pi_store::theme`), and so is what a
//! string occupies (`pi_store::text`); what is here puts the two on a screen.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::{IsTerminal, Write};
use std::ops::Range;
use std::sync::Arc;

use agent::Event;
use llm::model::Pricing;
use ratatui::text::{Line, Span};

use crate::sgr::{band_to_ratatui, style_to_ratatui};
use pi_store::icons;
use pi_store::text::{RESET, clip};
use pi_store::theme::{Attr, Color, Style, Theme, push_sep};

/// Whether the surface being written to can carry colour, and the theme behind
/// the codes it uses.
#[derive(Debug)]
pub struct Paint {
    pub color: bool,
    pub theme: Arc<Theme>,
}

impl Paint {
    #[cfg(test)]
    pub fn new(color: bool) -> Self {
        Self {
            color,
            theme: Arc::new(Theme::default()),
        }
    }

    pub fn with_theme(color: bool, theme: Arc<Theme>) -> Self {
        Self { color, theme }
    }

    pub fn on(&self, style: &Style, body: &str) -> String {
        if !self.color {
            return body.to_string();
        }
        let codes = style.codes();
        if codes.is_empty() {
            body.to_string()
        } else {
            format!("\x1b[{codes}m{body}{RESET}")
        }
    }

    /// `body` in `style`, as a ratatui span: the unit every row on the surface
    /// is built from. Styles flow structurally from here on — no SGR in the
    /// text, so wrapping and measuring need no escape scanning.
    pub fn span(&self, style: &Style, body: impl Into<String>) -> Span<'static> {
        if !self.color {
            Span::raw(body.into())
        } else {
            Span::styled(body.into(), style_to_ratatui(style))
        }
    }

    /// `body` in `style`, as a span, with bold added while hovered — hover
    /// strengthens the row without changing its colour, so a green check
    /// stays green and a grey body stays grey under the cursor.
    pub fn span_hovered(
        &self,
        hovered: bool,
        style: &Style,
        body: impl Into<String>,
    ) -> Span<'static> {
        if !hovered || !self.color {
            return self.span(style, body);
        }
        self.span(&style.lit(), body)
    }

    /// A drawn span's style under the pointer, as `Style::lit` has it.
    pub fn lit(style: ratatui::style::Style) -> ratatui::style::Style {
        style.add_modifier(ratatui::style::Modifier::BOLD)
    }

    /// The band `color` paints on this surface, or `None` on a surface that
    /// carries no colour at all — the two are the same decision.
    pub fn band(&self, color: &Color) -> Option<ratatui::style::Style> {
        self.color.then(|| band_to_ratatui(color))
    }
}

/// Render a ratatui Line into an ANSI-escaped string.
fn line_to_ansi(line: &ratatui::text::Line<'_>) -> String {
    let mut out = String::new();
    let mut params = String::new();
    let base_style = line.style;
    for span in &line.spans {
        let style = base_style.patch(span.style);
        params.clear();
        append_style_params(&mut params, style);
        if params.is_empty() {
            out.push_str(&span.content);
        } else {
            out.push_str("\x1b[");
            out.push_str(&params);
            out.push('m');
            out.push_str(&span.content);
            out.push_str(RESET);
        }
    }
    out
}

/// The printed form of a described event: each line on its own row.
fn lines_to_ansi(lines: &[ratatui::text::Line<'_>]) -> String {
    lines
        .iter()
        .map(line_to_ansi)
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_sgr(out: &mut String, code: u8) {
    push_sep(out);
    let _ = write!(out, "{code}");
}

fn append_color(out: &mut String, color: ratatui::style::Color, bg: bool) {
    use ratatui::style::Color as RColor;
    // The tables `parse_sgr` reads, run backwards: both ends speak one list.
    let (base, bright) = if bg { (40, 100) } else { (30, 90) };
    if let Some(i) = crate::sgr::NAMED.iter().position(|&c| c == color) {
        return write_sgr(out, base + i as u8);
    }
    if let Some(i) = crate::sgr::BRIGHT.iter().position(|&c| c == color) {
        return write_sgr(out, bright + i as u8);
    }
    match color {
        RColor::Reset => write_sgr(out, if bg { 49 } else { 39 }),
        RColor::Indexed(n) => {
            push_sep(out);
            let prefix = if bg { "48;5;" } else { "38;5;" };
            out.push_str(prefix);
            let _ = write!(out, "{n}");
        }
        RColor::Rgb(r, g, b) => {
            push_sep(out);
            let prefix = if bg { "48;2;" } else { "38;2;" };
            out.push_str(prefix);
            let _ = write!(out, "{r};{g};{b}");
        }
        _ => {}
    }
}

fn append_style_params(out: &mut String, style: ratatui::style::Style) {
    use ratatui::style::Modifier as RModifier;
    // Every attribute code `parse_sgr` reads: both ends speak one list.
    for (modifier, code) in [
        (RModifier::BOLD, 1),
        (RModifier::DIM, 2),
        (RModifier::ITALIC, 3),
        (RModifier::UNDERLINED, 4),
        (RModifier::SLOW_BLINK, 5),
        (RModifier::RAPID_BLINK, 6),
        (RModifier::REVERSED, 7),
        (RModifier::HIDDEN, 8),
        (RModifier::CROSSED_OUT, 9),
    ] {
        if style.add_modifier.contains(modifier) {
            write_sgr(out, code);
        }
    }
    if let Some(fg) = style.fg {
        append_color(out, fg, false);
    }
    if let Some(bg) = style.bg {
        append_color(out, bg, true);
    }
}

// Stands in for a code block's fence lines, so `fenced` can find them after
// rendering: a private-use char no model writes.
const FENCE: &str = "\u{E000}";

/// A code block's head: its language on the prompt's band, and `note` muted
/// after it.
pub fn badge(lang: &str, note: &str, paint: &Paint) -> Line<'static> {
    let band =
        style_to_ratatui(&paint.theme.code).patch(band_to_ratatui(&paint.theme.prompt.panel.said));
    badge_with(lang, note, band, style_to_ratatui(&paint.theme.muted))
}

fn badge_with(
    lang: &str,
    note: &str,
    band: ratatui::style::Style,
    muted: ratatui::style::Style,
) -> Line<'static> {
    let lang = if lang.is_empty() { "block" } else { lang };
    let mut spans = vec![Span::styled(format!(" {lang} "), band)];
    if !note.is_empty() {
        spans.push(Span::styled(format!(" {note}"), muted));
    }
    Line::from(spans)
}

// A code block's opening fence becomes its badge, carrying the block's text;
// the rest of its info string is the note, and the closing fence goes.
fn fenced(
    lines: Vec<(Line<'static>, crate::unit::Units)>,
    badge: ratatui::style::Style,
    muted: ratatui::style::Style,
) -> Vec<Rendered> {
    let mut open: Option<(usize, Vec<String>)> = None;
    let mut out: Vec<Rendered> = Vec::with_capacity(lines.len());
    for (line, units) in lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let Some(lang) = text.strip_prefix(FENCE) else {
            let coded = match &mut open {
                Some((_, body)) => {
                    body.push(text);
                    Coded::Code
                }
                None => Coded::Text,
            };
            out.push(Rendered { line, coded, units });
            continue;
        };
        match open.take() {
            Some((at, body)) => out[at].coded = Coded::Badge(body.join("\n")),
            None => {
                let (lang, note) = lang.trim().split_once(' ').unwrap_or((lang.trim(), ""));
                open = Some((out.len(), Vec::new()));
                out.push(Rendered {
                    line: badge_with(lang, note.trim(), badge, muted),
                    coded: Coded::Badge(String::new()),
                    units: Vec::new(),
                });
            }
        }
    }
    if let Some((at, body)) = open {
        out[at].coded = Coded::Badge(body.join("\n"));
    }
    out
}

fn trim_partial_fences(text: &str) -> &str {
    for suffix in ["\n`", "\n``"] {
        if let Some(rest) = text.strip_suffix(suffix)
            && !rest.ends_with('`')
        {
            return rest;
        }
    }
    text
}

#[derive(Debug, Clone)]
struct PiStyleSheet {
    heading: ratatui::style::Style,
    code: ratatui::style::Style,
    muted: ratatui::style::Style,
}

impl tui_markdown::StyleSheet for PiStyleSheet {
    fn heading(&self, _level: u8) -> ratatui::style::Style {
        self.heading
    }

    fn code(&self) -> ratatui::style::Style {
        self.code
    }

    fn code_block_fence(&self) -> &str {
        FENCE
    }

    fn link(&self) -> ratatui::style::Style {
        self.code
    }

    fn blockquote(&self) -> ratatui::style::Style {
        self.muted
    }

    fn heading_meta(&self) -> ratatui::style::Style {
        self.muted
    }

    fn table_header(&self) -> ratatui::style::Style {
        self.heading
    }

    fn table_border(&self) -> ratatui::style::Style {
        self.muted
    }

    fn image_alt(&self) -> ratatui::style::Style {
        self.muted
    }
}

/// What a rendered line is to the code blocks around it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coded {
    Text,
    /// A block's badge, carrying the block's text for a click to copy.
    Badge(String),
    /// A line inside a block: set off from the prose around it, so where
    /// that stops is where the block ends.
    Code,
}

/// A rendered line, what it is to a code block, and its stretches that
/// read as one.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub line: Line<'static>,
    pub coded: Coded,
    pub units: crate::unit::Units,
}

/// Parse and render markdown into styled lines using `tui-markdown` and theme.
/// Styles ride on the spans; nothing here speaks SGR.
pub fn render_markdown(text: &str, paint: &Paint) -> Vec<ratatui::text::Line<'static>> {
    render_coded(text, paint)
        .into_iter()
        .map(|r| r.line)
        .collect()
}

/// The same lines, each with what it is to a code block and what in it
/// reads as one.
pub fn render_coded(text: &str, paint: &Paint) -> Vec<Rendered> {
    if text.is_empty() {
        return Vec::new();
    }
    // The fences stay in the text, so they still mark where a block ends.
    if !paint.color {
        return text
            .lines()
            .map(|l| Rendered {
                line: Line::from(l.to_string()),
                coded: Coded::Text,
                units: Vec::new(),
            })
            .collect();
    }
    let trimmed = trim_partial_fences(text);
    // Drawn for the terminal as it is now; a narrower one gets the source.
    let width = crossterm::terminal::size()
        .ok()
        .map(|(w, _)| usize::from(w));
    let drawn = crate::block::drawn(trimmed, width);
    let marked = crate::unit::mark(&drawn);
    let sheet = PiStyleSheet {
        heading: style_to_ratatui(&paint.theme.heading),
        code: style_to_ratatui(&paint.theme.code),
        muted: style_to_ratatui(&paint.theme.muted),
    };
    let badge = sheet
        .code
        .patch(band_to_ratatui(&paint.theme.prompt.panel.said));
    let muted = sheet.muted;
    let options = tui_markdown::Options::new(sheet);
    let parsed = tui_markdown::from_str_with_options(&marked, &options);
    // The parsed text borrows the input; flatten text/line styles onto the
    // spans and own the content, so the lines outlive this call.
    let lines = parsed
        .lines
        .into_iter()
        .map(|line| {
            let line_style = parsed.style.patch(line.style);
            let spans: Vec<Span<'static>> = line
                .spans
                .into_iter()
                .map(|span| Span::styled(span.content.to_string(), line_style.patch(span.style)))
                .collect();
            Line::from(spans)
        })
        .collect();
    let lines = match marked {
        Cow::Owned(_) => crate::unit::strip(lines),
        Cow::Borrowed(_) => lines.into_iter().map(|l| (l, Vec::new())).collect(),
    };
    fenced(lines, badge, muted)
}

/// The diff rows a sketched (folded) result shows under its head.
const SKETCH_LIMIT: usize = 16;
/// The preview rows a folded result shows: the head plus the sketch limit.
pub const SKETCHED_ROWS: usize = 1 + SKETCH_LIMIT;

/// Whether `rows` preview rows, head included, fold to `SKETCHED_ROWS`.
pub fn folds(rows: usize) -> bool {
    llm::figures::shown(rows, SKETCHED_ROWS) < rows
}

/// How much of a result its rows show: the sketch, or the whole of it in
/// place or on a page of its own, where there is nothing to fold back.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    Sketch,
    Open,
    Page,
}

/// The columns a result's lines are cut to at `width`.
pub fn result_room(width: usize) -> usize {
    width.saturating_sub(2).max(20)
}

/// How a finished call came out, which its mark says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    // Ran to the end, but what came back deserves a look.
    Warned,
    Failed,
}

impl Outcome {
    pub fn of(is_error: bool, warned: bool) -> Self {
        match (is_error, warned) {
            (true, _) => Self::Failed,
            (false, true) => Self::Warned,
            (false, false) => Self::Done,
        }
    }

    /// The mark and the style it is drawn in.
    pub fn mark(self, theme: &Theme) -> (&Style, &'static str) {
        match self {
            Self::Done => (&theme.status.ok, icons::DONE_MARK),
            Self::Warned => (&theme.status.warn, icons::DONE_MARK),
            Self::Failed => (&theme.status.err, icons::FAIL_MARK),
        }
    }
}

/// A finished call, as its result row draws it.
pub struct Finished<'a> {
    pub outcome: Outcome,
    pub name: &'a str,
    pub preview: &'a str,
    /// Where it ran, when not the workspace root.
    pub place: Option<&'a str>,
}

/// The rows a finished tool result takes on screen: its head, clipped to fit,
/// and under it whatever a tool sketched — an edit's diff rows.
pub fn result_rows(
    call: &Finished,
    shown: Shown,
    hovered: bool,
    p: &Paint,
    width: usize,
) -> Vec<ratatui::text::Line<'static>> {
    let room = result_room(width);
    let (mark_style, mark) = call.outcome.mark(&p.theme);
    let (head, rest) = call.preview.split_once('\n').unwrap_or((call.preview, ""));
    // Past the mark, the name and the `…` a cut adds; the place is kept
    // whole, the command gives way to it.
    let placed = placed(call.place);
    let width_of = pi_store::text::width;
    let fits = room.saturating_sub(width_of(call.name) + 2 + width_of(placed.as_str()));
    let mut out = vec![Line::from(vec![
        p.span(mark_style, mark),
        Span::raw(format!(" {} ", call.name)),
        p.span_hovered(hovered, &p.theme.muted, clip(head, fits) + &placed),
    ])];
    let diff_lines: Vec<&str> = rest.lines().collect();
    let footer = |text: &str| Line::from(p.span_hovered(hovered, &p.theme.muted, text));
    let folded = folds(1 + diff_lines.len());
    let sketched = folded && shown == Shown::Sketch;
    let rows = if sketched {
        &diff_lines[..SKETCH_LIMIT]
    } else {
        &diff_lines[..]
    };
    out.extend(sketch_rows(rows, p, room));
    if sketched {
        out.push(footer(&format!(
            "  {} more",
            diff_lines.len() - SKETCH_LIMIT
        )));
    } else if folded && shown == Shown::Open {
        out.push(footer("  click to collapse"));
    }
    out
}

/// The rows of a sketch, each under the sign that opens it, and the changed
/// run of a rewritten line reversed inside both of its rows.
fn sketch_rows(rows: &[&str], p: &Paint, room: usize) -> Vec<Line<'static>> {
    let sign = |row: &str| row.chars().next().unwrap_or(' ');
    let mut out = Vec::with_capacity(rows.len());
    let mut runs = rows.chunk_by(|a, b| sign(a) == sign(b)).peekable();
    while let Some(run) = runs.next() {
        // One row gone and one come back is a line rewritten, and the two are
        // read together; a wider block has no counterpart to pair.
        let one_each = sign(run[0]) == '-'
            && run.len() == 1
            && runs
                .peek()
                .is_some_and(|next| sign(next[0]) == '+' && next.len() == 1);
        if one_each {
            let next = runs.next().expect("peeked");
            out.extend(replaced(run[0], next[0], p, room));
            continue;
        }
        let style = match sign(run[0]) {
            '+' => &p.theme.diff.add,
            '-' => &p.theme.diff.del,
            _ => &p.theme.muted,
        };
        for row in run {
            out.push(Line::from(p.span(style, format!("  {}", clip(row, room)))));
        }
    }
    out
}

/// The two rows of a rewritten line: the sign and number in the row's own
/// colour, and the run the two do not share reversed on top of it.
fn replaced(was: &str, is: &str, p: &Paint, room: usize) -> [Line<'static>; 2] {
    // The lead is the sign and the number, never the change: the number is
    // right-aligned, so its space is the first one with a digit behind it.
    let lead = was
        .char_indices()
        .find(|(at, c)| *c == ' ' && was[..*at].bytes().any(|b| b.is_ascii_digit()))
        .map_or(was.len(), |(at, _)| at + 1);
    let (was_body, is_body) = (
        clip(&was[lead..], room.saturating_sub(lead)),
        clip(&is[lead..], room.saturating_sub(lead)),
    );
    let (from, to) = diverged(&was_body, &is_body);
    let row = |head: &str, body: &str, (start, end): (usize, usize), style: &Style| {
        Line::from(vec![
            p.span(style, format!("  {head}")),
            p.span(style, body[..start].to_string()),
            p.span(&style.adding(Attr::Reverse), body[start..end].to_string()),
            p.span(style, body[end..].to_string()),
        ])
    };
    [
        row(
            &was[..lead],
            &was_body,
            (from.start, from.end),
            &p.theme.diff.del,
        ),
        row(&is[..lead], &is_body, (to.start, to.end), &p.theme.diff.add),
    ]
}

/// The run two rows do not share, as byte ranges into each: what is left of
/// them once the head and the tail they have in common are set aside.
fn diverged(was: &str, is: &str) -> (Range<usize>, Range<usize>) {
    let head = was
        .chars()
        .zip(is.chars())
        .take_while(|(a, b)| a == b)
        .count();
    // Cut the head off first and count the tail in what is left: the two runs
    // then cannot overlap, however alike the rows are.
    let at = |s: &str| s.char_indices().nth(head).map_or(s.len(), |(i, _)| i);
    let (was_at, is_at) = (at(was), at(is));
    let (was_rest, is_rest) = (&was[was_at..], &is[is_at..]);
    let tail = was_rest
        .chars()
        .rev()
        .zip(is_rest.chars().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let cut = |rest: &str| match tail {
        0 => rest.len(),
        n => {
            rest.char_indices()
                .rev()
                .nth(n - 1)
                .expect("counted from here")
                .0
        }
    };
    (was_at..was_at + cut(was_rest), is_at..is_at + cut(is_rest))
}

/// `at`, in wall-clock seconds, as the local time of day; on a day other
/// than `now`'s, led by its date.
pub fn clock(at: u64, now: u64) -> String {
    let (Some(when), Some(today)) = (local(at), local(now)) else {
        return String::new();
    };
    let time = format!("{:02}:{:02}", when.tm_hour, when.tm_min);
    if (when.tm_year, when.tm_yday) == (today.tm_year, today.tm_yday) {
        time
    } else {
        format!("{:02}-{:02} {time}", when.tm_mon + 1, when.tm_mday)
    }
}

fn local(secs: u64) -> Option<libc::tm> {
    let t = libc::time_t::try_from(secs).ok()?;
    // SAFETY: `tm` is plain data, and `localtime_r` writes only into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let filled = unsafe { libc::localtime_r(&t, &mut tm) };
    (!filled.is_null()).then_some(tm)
}

pub fn fmt_delay(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

/// A run's line for one event, plus detail rows for a tool that offers them —
/// one `Line` per screen row, caller's to place.
///
/// Shared by both renderers so tool wording never drifts between them.
/// `None` covers the two deltas and `Done`, which the surface composes itself.
pub fn describe(
    event: &Event,
    p: &Paint,
    width: usize,
) -> Option<Vec<ratatui::text::Line<'static>>> {
    let room = width.saturating_sub(2).max(20);
    let line = match event {
        // Same naming every line that shows a call uses — not a place to
        // spell a tool differently.
        Event::ToolStart { name, args, .. } => Line::from(vec![
            p.span(&p.theme.muted, icons::PENDING_MARK),
            Span::raw(format!(" {}", named(name, &summarize(args)))),
        ]),
        Event::ToolEnd {
            name,
            is_error,
            warned,
            preview,
            ..
        } => {
            let call = Finished {
                outcome: Outcome::of(*is_error, *warned),
                name,
                preview,
                place: None,
            };
            return Some(result_rows(&call, Shown::Sketch, false, p, width));
        }
        Event::ToolDenied { name, reason, .. } => Line::from(vec![
            p.span(&p.theme.status.err, icons::FAIL_MARK),
            Span::raw(format!(" {name} ")),
            p.span(&p.theme.muted, clip(reason, room)),
        ]),
        Event::Compacted(r) => {
            Line::from(p.span(&p.theme.muted, pi_core::core::meter::compaction_line(r)))
        }
        // Clipped whole, prefix and reason: a reason cut to the width alone
        // lands in a bar that cuts it again, in silence and without the ellipsis.
        Event::Retrying {
            attempt,
            delay_ms,
            reason,
        } => Line::from(p.span(
            &p.theme.muted,
            clip(
                &format!(
                    "retry {attempt} in {}{}{reason}",
                    fmt_delay(*delay_ms),
                    icons::PART_SEP
                ),
                room,
            ),
        )),
        Event::Warning(w) | Event::Refit(w) => Line::from(p.span(&p.theme.muted, w)),
        // Done is a status line rather than an event's wording, and each
        // renderer composes it from its own configured segments.
        _ => return None,
    };
    Some(vec![line])
}

pub struct Renderer {
    paint: Paint,
    quiet: bool,
    // The segments this renderer ends a run with. A one-shot run times nothing
    // and queues nothing, so `elapsed` and `queued` have nothing to say here.
    status: Vec<pi_store::status::Segment>,
    // Read off the same events the terminal reads, so a one-shot run ends on
    // the line the terminal would have shown it.
    tally: pi_core::core::meter::Tally,
    model: String,

    // The worktree this run is working in, for the segment that names it.
    worktree: Option<String>,
    thinking: bool,
    // Each stream is tracked separately: they share a terminal when both are
    // a tty, but only the dirty one may be terminated when piped apart.
    out_dirty: bool,
    err_dirty: bool,
}

impl Renderer {
    pub fn new(
        quiet: bool,
        theme: Arc<Theme>,
        status: Vec<pi_store::status::Segment>,
        model: String,
        pricing: Pricing,
        worktree: Option<String>,
    ) -> Self {
        // Priced once: a one-shot run cannot switch models, so the rate the
        // events are added up at is the rate it started with.
        let mut tally = pi_core::core::meter::Tally::default();
        tally.set_pricing(pricing);
        Self {
            paint: Paint::with_theme(std::io::stderr().is_terminal(), theme),
            quiet,
            status,
            tally,
            model,
            worktree,
            thinking: false,
            out_dirty: false,
            err_dirty: false,
        }
    }

    /// Answer text goes to stdout so it pipes; everything else is progress and
    /// goes to stderr.
    pub fn on(&mut self, event: Event) {
        self.tally.on(&event);
        match &event {
            Event::ReasoningDelta(d) if !self.quiet => {
                if !self.thinking {
                    self.settle_out();
                    eprint!("{}", self.paint.on(&self.paint.theme.muted, "thinking "));
                    self.thinking = true;
                }
                eprint!("{}", self.paint.on(&self.paint.theme.muted, d));
                self.err_dirty = true;
                let _ = std::io::stderr().flush();
            }
            Event::TextDelta(d) => {
                self.end_thinking();
                self.settle_err();
                print!("{d}");
                self.out_dirty = !d.ends_with('\n');
                let _ = std::io::stdout().flush();
            }
            Event::Done { .. } if !self.quiet => {
                self.end_thinking();
                self.settle();
                let snap = self
                    .tally
                    .snapshot(&self.model, self.worktree.as_deref(), None, 0);
                let line = crate::status::line(&self.status, &snap);
                if !line.is_empty() {
                    eprintln!("{}", self.paint.on(&self.paint.theme.muted, &line));
                }
            }
            // Worth seeing even under --quiet: the run did less than it was asked.
            Event::ToolDenied { .. } => {
                self.settle();
                if let Some(lines) = describe(&event, &self.paint, 100) {
                    eprintln!("{}", lines_to_ansi(&lines));
                }
            }
            _ if self.quiet => {}
            _ => {
                if let Some(lines) = describe(&event, &self.paint, 100) {
                    self.end_thinking();
                    self.settle();
                    eprintln!("{}", lines_to_ansi(&lines));
                }
            }
        }
    }

    fn end_thinking(&mut self) {
        self.thinking = false;
    }

    // Terminate the answer stream's partial line. Never called between two
    // text deltas: they continue one line, they do not each start one.
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

    // Before a whole-line write, which must start at column zero on both.
    fn settle(&mut self) {
        self.settle_out();
        self.settle_err();
    }

    pub fn finish(&mut self) {
        self.end_thinking();
        self.settle();
    }
}

/// A tool name plus the one argument its summary picked out; the caller
/// draws the prefix (spinner, ⚙, or a finished mark) itself.
pub fn named(name: &str, summary: &str) -> String {
    if summary.is_empty() {
        name.to_string()
    } else {
        format!("{name} {summary}")
    }
}

/// The one argument worth showing, whole.
pub fn asked(args: &serde_json::Value) -> &str {
    // Order is priority: `pattern` beats `path` because a grep carries both,
    // and `description`, written for this line, beats the prompt it names.
    [
        "description",
        "pattern",
        "command",
        "path",
        "query",
        "prompt",
        "name",
    ]
    .into_iter()
    .find_map(|key| args.get(key).and_then(|v| v.as_str()))
    .unwrap_or_default()
}

/// Where a call ran, when that is not the workspace `root`: the `cwd` it
/// named, relative to `root` when it lies inside.
pub fn place(args: &serde_json::Value, root: &std::path::Path) -> Option<String> {
    let cwd = args.get("cwd")?.as_str()?.trim();
    let cwd = std::path::Path::new(cwd)
        .strip_prefix(root)
        .ok()
        .and_then(|p| p.to_str())
        .unwrap_or(cwd);
    let cwd = cwd.strip_prefix("./").unwrap_or(cwd);
    // `/` alone is a place, not a root spelled with a trailing slash.
    let cwd = match cwd.trim_end_matches('/') {
        "" => cwd,
        trimmed => trimmed,
    };
    (!cwd.is_empty() && cwd != ".").then(|| cwd.to_string())
}

/// `place` as the tail of the call's own line, ` · in crates/ui`; a row
/// speaking for many calls leaves it off.
pub fn placed(place: Option<&str>) -> String {
    place.map_or_else(String::new, |p| format!("{}in {p}", icons::PART_SEP))
}

/// The one argument worth showing in a progress line: its first line, `…`
/// saying there is more of it.
pub fn summarize(args: &serde_json::Value) -> String {
    let v = asked(args);
    let first = v.lines().next().unwrap_or_default();
    let line = clip(first, 80);
    if first.len() < v.trim_end().len() && !line.ends_with(icons::ELLIPSIS) {
        format!("{line}{}", icons::ELLIPSIS)
    } else {
        line
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn consecutive_text_deltas_stay_on_one_line() {
        let mut r = super::Renderer::new(
            false,
            std::sync::Arc::new(super::Theme::default()),
            pi_store::status::default_parts(),
            String::new(),
            llm::model::Pricing::default(),
            None,
        );
        r.on(agent::Event::TextDelta("There".into()));
        assert!(r.out_dirty, "an unterminated delta leaves the line open");
        r.on(agent::Event::TextDelta("'s a bug".into()));
        // settle_out must not fire between deltas, or every token gets its own line.
        assert!(r.out_dirty);
        r.on(agent::Event::TextDelta("done\n".into()));
        assert!(!r.out_dirty, "a delta ending in a newline closes the line");
    }

    // A click copies this text, so drift in the parser would land in pastes.
    #[test]
    fn a_code_blocks_badge_carries_its_text_verbatim() {
        let code = "fn main() {\n    let x = 1;\n\n\tx\n}";
        let text = format!("before\n\n```rust\n{code}\n```\n\nafter");
        let coded = super::render_coded(&text, &super::Paint::new(true));
        let carried: Vec<_> = coded
            .iter()
            .filter_map(|r| match &r.coded {
                super::Coded::Badge(code) => Some(code.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(carried, [code]);
        // Every line of the block, and nothing past it, is set off as code.
        let set_off = coded
            .iter()
            .filter(|r| r.coded == super::Coded::Code)
            .count();
        assert_eq!(set_off, code.lines().count());
    }

    // The root's own path, or a sibling sharing its prefix, would read wrong.
    #[test]
    fn an_absolute_cwd_reads_relative_to_the_root() {
        let root = std::path::Path::new("/w/pi");
        let at = |cwd: &str| super::place(&serde_json::json!({ "cwd": cwd }), root);
        assert_eq!(at("/w/pi"), None);
        assert_eq!(at("/w/pi/"), None);
        assert_eq!(at("/w/pi/crates/ui"), Some("crates/ui".into()));
        assert_eq!(at("/w/pix"), Some("/w/pix".into()));
        assert_eq!(at("crates"), Some("crates".into()));
    }
}
