//! Drawing: what a painted span is, how a theme's style becomes one, and the
//! line-mode renderer that writes rows into a pipe.
//!
//! What a config names is below this (`store/theme.rs`), and so is what a
//! string occupies (`store/text.rs`); what is here puts the two on a screen.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write};
use std::ops::Range;
use std::sync::Arc;

use agent::Event;
use ratatui::text::{Line, Span};

use crate::store::icons;
use crate::store::text::{RESET, clip, summarize};
use crate::store::theme::{Attr, Style, Theme, push_sep, style_to_ratatui};

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
        self.span(&style.adding(Attr::Bold), body)
    }
}

/// Render a ratatui Line into an ANSI-escaped string.
pub fn line_to_ansi(line: &ratatui::text::Line<'_>) -> String {
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

/// The pipe-side form of a described event: each line on its own row.
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
    let base = if bg { 40 } else { 30 };
    let bright = if bg { 100 } else { 90 };
    match color {
        RColor::Reset => write_sgr(out, if bg { 49 } else { 39 }),
        RColor::Black => write_sgr(out, base),
        RColor::Red => write_sgr(out, base + 1),
        RColor::Green => write_sgr(out, base + 2),
        RColor::Yellow => write_sgr(out, base + 3),
        RColor::Blue => write_sgr(out, base + 4),
        RColor::Magenta => write_sgr(out, base + 5),
        RColor::Cyan => write_sgr(out, base + 6),
        // Gray is the eighth named colour, DarkGray the bright black; keeping
        // them apart is what makes SGR 37 survive a row and a block alike.
        RColor::Gray => write_sgr(out, base + 7),
        RColor::DarkGray => write_sgr(out, bright),
        RColor::LightRed => write_sgr(out, bright + 1),
        RColor::LightGreen => write_sgr(out, bright + 2),
        RColor::LightYellow => write_sgr(out, bright + 3),
        RColor::LightBlue => write_sgr(out, bright + 4),
        RColor::LightMagenta => write_sgr(out, bright + 5),
        RColor::LightCyan => write_sgr(out, bright + 6),
        RColor::White => write_sgr(out, bright + 7),
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

/// Parse and render markdown into styled lines using `tui-markdown` and theme.
/// Styles ride on the spans; nothing here speaks SGR.
pub fn render_markdown(text: &str, paint: &Paint) -> Vec<ratatui::text::Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    if !paint.color {
        return text.lines().map(|l| Line::from(l.to_string())).collect();
    }
    let trimmed = trim_partial_fences(text);
    let sheet = PiStyleSheet {
        heading: style_to_ratatui(&paint.theme.heading),
        code: style_to_ratatui(&paint.theme.code),
        muted: style_to_ratatui(&paint.theme.muted),
    };
    let options = tui_markdown::Options::new(sheet);
    let parsed = tui_markdown::from_str_with_options(trimmed, &options);
    // The parsed text borrows the input; flatten text/line styles onto the
    // spans and own the content, so the lines outlive this call.
    parsed
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
        .collect()
}

/// The diff rows a sketched (folded) result shows under its head.
pub const SKETCH_LIMIT: usize = 24;
/// The preview rows a folded result shows: the head plus the sketch limit.
pub const SKETCHED_ROWS: usize = 1 + SKETCH_LIMIT;

/// The rows a finished tool result takes on screen: its head, clipped to fit,
/// and under it whatever a tool sketched — an edit's diff rows.
pub fn result_rows(
    is_error: bool,
    name: &str,
    preview: &str,
    expanded: bool,
    hovered: bool,
    p: &Paint,
    width: usize,
) -> Vec<ratatui::text::Line<'static>> {
    let room = width.saturating_sub(2).max(20);
    let (mark_style, mark) = if is_error {
        (&p.theme.status.err, icons::FAIL_MARK)
    } else {
        (&p.theme.status.ok, icons::DONE_MARK)
    };
    let (head, rest) = preview.split_once('\n').unwrap_or((preview, ""));
    let mut out = vec![Line::from(vec![
        p.span(mark_style, mark),
        Span::raw(format!(" {name} ")),
        p.span(&p.theme.muted, clip(head, room)),
    ])];
    let diff_lines: Vec<&str> = rest.lines().collect();
    let footer = |text: &str| Line::from(p.span_hovered(hovered, &p.theme.muted, text));
    let sketched = diff_lines.len() > SKETCH_LIMIT && !expanded;
    let shown = if sketched {
        &diff_lines[..SKETCH_LIMIT]
    } else {
        &diff_lines[..]
    };
    out.extend(sketch_rows(shown, p, room));
    if sketched {
        out.push(footer(&format!(
            "  {} {} more",
            icons::ELLIPSIS,
            diff_lines.len() - SKETCH_LIMIT
        )));
    } else if diff_lines.len() > SKETCH_LIMIT {
        out.push(footer("  ▴ collapse"));
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

fn fmt_delay(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

/// The wording for every event that occupies a whole line.
///
/// Both surfaces call this: a tool call has to read the same in a pipe as in
/// the terminal, and two copies of the wording would drift on the first edit.
/// None is the caller's to place: the two deltas, which are a fragment rather
/// than a line, and `Done`, which is a status line the surface composes itself.
/// A run's line for one event, and for a tool that offers one, the rows of
/// detail under it — one `Line` per screen row, because the caller decides
/// what a row is: the interactive surface repaints a region and hands them
/// over one at a time.
pub fn describe(
    event: &Event,
    p: &Paint,
    width: usize,
) -> Option<Vec<ratatui::text::Line<'static>>> {
    let room = width.saturating_sub(2).max(20);
    let line = match event {
        Event::ToolStart { name, args, .. } => Line::from(vec![
            p.span(&p.theme.muted, icons::PENDING_MARK),
            Span::raw(format!(" {name} ")),
            p.span(&p.theme.muted, summarize(args)),
        ]),
        Event::ToolEnd {
            name,
            is_error,
            preview,
            ..
        } => {
            return Some(result_rows(
                *is_error, name, preview, false, false, p, width,
            ));
        }
        Event::ToolDenied { name, reason, .. } => Line::from(vec![
            p.span(&p.theme.status.err, icons::FAIL_MARK),
            Span::raw(format!(" {name} ")),
            p.span(&p.theme.muted, clip(reason, room)),
        ]),
        Event::Compacted(r) => Line::from(p.span(&p.theme.muted, compaction_line(r))),
        Event::Retrying {
            attempt,
            delay_ms,
            reason,
        } => Line::from(p.span(
            &p.theme.muted,
            format!(
                "retry {attempt} in {}{}{}",
                fmt_delay(*delay_ms),
                icons::PART_SEP,
                clip(reason, room)
            ),
        )),
        Event::Warning(w) => Line::from(vec![
            p.span(&p.theme.status.err, icons::WARN_MARK),
            Span::raw(" "),
            p.span(&p.theme.muted, w),
        ]),
        // Done is a status line rather than an event's wording, and the two
        // surfaces render it from their own configured segments.
        _ => return None,
    };
    Some(vec![line])
}

pub struct Renderer {
    paint: Paint,
    quiet: bool,
    // The segments this surface ends a run with. A pipe times nothing and
    // queues nothing, so `elapsed` and `queued` have nothing to say here.
    done: Vec<crate::store::status::Segment>,
    // Read off the same events the terminal reads, so a piped run ends on the
    // line the terminal would have shown it.
    tally: crate::run::meter::Tally,
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
        done: Vec<crate::store::status::Segment>,
        model: String,
        worktree: Option<String>,
    ) -> Self {
        Self {
            paint: Paint::with_theme(std::io::stderr().is_terminal(), theme),
            quiet,
            done,
            tally: crate::run::meter::Tally::default(),
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
        // Before the arms and outside the `quiet` guards: a run still has to
        // arrive at the right total when nothing about it was printed.
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
                let line = crate::ui::status::line(&self.done, &snap);
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

// Says what was given up, not just how much. A silent shrink looks like the
// agent forgetting things for no reason.
fn compaction_line(r: &agent::Report) -> String {
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
    if r.args_taken > 0 {
        parts.push(format!("{} arguments taken", r.args_taken));
    }

    if r.notices_pruned > 0 {
        parts.push(format!("{} notices pruned", r.notices_pruned));
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
        format!("{}{}", icons::PART_SEP, parts.join(", "))
    };
    let warn = if r.still_over {
        format!("{}still over budget", icons::PART_SEP)
    } else {
        String::new()
    };
    format!("compacted {} → {} tokens{detail}{warn}", r.before, r.after)
}

#[cfg(test)]
mod tests {
    #[test]
    fn consecutive_text_deltas_stay_on_one_line() {
        let mut r = super::Renderer::new(
            false,
            std::sync::Arc::new(super::Theme::default()),
            crate::store::status::default_done(),
            String::new(),
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
}
