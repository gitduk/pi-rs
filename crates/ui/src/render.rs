//! Drawing: what a painted span is, how a theme's style becomes one, and the
//! renderer that writes a one-shot run's rows.
//!
//! What a config names is below this (`pi_store::theme`), and so is what a
//! string occupies (`pi_store::text`); what is here puts the two on a screen.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::{IsTerminal, Write};
use std::sync::{Arc, OnceLock};

use agent::Event;
use llm::model::Pricing;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};

mod diff;

use crate::markdown::FENCE;
use crate::sgr::{band_to_ratatui, style_to_ratatui};
#[cfg(test)]
use diff::rows as sketch_rows;
use pi_store::icons;
use pi_store::text::{RESET, clip};
use pi_store::theme::{Color, Style, Theme, push_sep};

// A heading's mark, text and trailing metadata share one allocation rule.
pub(crate) fn title_room(width: usize, lead: usize, tail: &str) -> usize {
    if width > u16::MAX as usize {
        return width;
    }
    let [_, body, _] = Layout::horizontal([
        Constraint::Length(lead.min(u16::MAX as usize) as u16),
        Constraint::Fill(1),
        Constraint::Length(Line::raw(tail).width().min(u16::MAX as usize) as u16),
    ])
    .areas(Rect::new(0, 0, width as u16, 1));
    body.width as usize
}

/// Whether the surface being written to can carry colour, and the theme behind
/// the codes it uses.
#[derive(Debug)]
pub struct Paint {
    pub color: bool,
    pub theme: Arc<Theme>,
    foreground: Option<(u8, u8, u8)>,
    background: Option<(u8, u8, u8)>,
    // Parsing an embedded theme is startup work, never per streaming delta.
    markdown: OnceLock<crate::markdown::Format>,
}

impl Paint {
    #[cfg(test)]
    pub fn new(color: bool) -> Self {
        Self::with_theme(color, Arc::new(Theme::default()))
    }

    pub fn with_theme(color: bool, theme: Arc<Theme>) -> Self {
        Self {
            color,
            theme,
            foreground: None,
            background: None,
            markdown: OnceLock::new(),
        }
    }

    pub(crate) fn with_background(mut self, background: Option<(u8, u8, u8)>) -> Self {
        self.background = background;
        self
    }

    pub(crate) fn with_foreground(mut self, foreground: Option<(u8, u8, u8)>) -> Self {
        self.foreground = foreground;
        self
    }

    /// Give reasoning its own secondary foreground so syntax colours cannot carry over.
    pub fn reasoning_style(&self) -> Style {
        crate::theme::secondary_text(self.foreground, self.background)
    }

    /// A tool's existing display text, coloured without changing its wording.
    pub fn tool_call(&self, name: &str, text: &str, hovered: bool) -> Vec<Span<'static>> {
        if !self.color {
            return vec![Span::raw(text.to_string())];
        }
        let Some(args) = text.strip_prefix(name) else {
            // A narrow row can cut even the name itself.
            return vec![self.span_hovered(hovered, &self.theme.prompt.color, text)];
        };
        let mut spans = vec![self.span_hovered(hovered, &self.theme.prompt.color, name)];
        let args = if name == toolbox::bash::Bash::NAME {
            self.shell(args, hovered)
        } else {
            vec![self.span_hovered(hovered, &self.theme.code, args)]
        };
        spans.extend(args);
        spans
    }

    /// A shell command, with the same syntax colours on every surface.
    pub(super) fn shell(&self, text: &str, hovered: bool) -> Vec<Span<'static>> {
        if !self.color {
            return vec![Span::raw(text.to_string())];
        }
        let mut spans = crate::markdown::shell(text, self.theme.syntax);
        if hovered {
            for span in &mut spans {
                span.style = Self::lit(span.style);
            }
        }
        spans
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

    /// Keep explicit foregrounds legible against the terminal background.
    pub(crate) fn readable_span(&self, style: &Style, body: impl Into<String>) -> Span<'static> {
        let mut span = self.span(style, body);
        if self.color {
            span.style = crate::theme::readable(span.style, self.background);
        }
        span
    }

    /// Blend decorative foregrounds into the measured terminal background.
    pub(crate) fn faded(&self, style: &Style, strength: u16) -> ratatui::style::Style {
        use ratatui::style::{Color, Modifier, Style as RStyle};

        if !self.color {
            return RStyle::default();
        }
        let style = style_to_ratatui(style);
        let foreground = match style.fg {
            Some(Color::Rgb(r, g, b)) => Some((r, g, b)),
            None | Some(Color::Reset) => self.foreground,
            _ => None,
        };
        match foreground.zip(self.background) {
            Some((foreground, background)) => {
                let (r, g, b) = crate::theme::blend(foreground, background, strength.min(100));
                style.fg(Color::Rgb(r, g, b)).remove_modifier(Modifier::DIM)
            }
            None => style.add_modifier(Modifier::DIM),
        }
    }

    /// `body` in the muted voice, a line per line it holds.
    pub fn muted(&self, body: &str) -> ratatui::text::Text<'static> {
        (body.lines())
            .map(|l| Line::from(self.span(&self.theme.muted, l)))
            .collect()
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

    /// A clickable hint in the link colour, with the same hover as its row.
    pub fn action_hint(&self, hovered: bool, body: impl Into<String>) -> Span<'static> {
        let mut span = self.span_hovered(hovered, &self.theme.link, body);
        if self.color {
            span.style = span
                .style
                .remove_modifier(ratatui::style::Modifier::UNDERLINED);
        }
        span
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
    lines: Vec<Rendered>,
    badge: ratatui::style::Style,
    muted: ratatui::style::Style,
) -> Vec<Rendered> {
    let mut open: Option<(usize, Vec<String>)> = None;
    let mut out: Vec<Rendered> = Vec::with_capacity(lines.len());
    for Rendered {
        line,
        units,
        prefix,
        cells,
        ..
    } in lines
    {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let Some(lang) = text.strip_prefix(FENCE) else {
            let coded = match &mut open {
                Some((_, body)) => {
                    body.push(text);
                    Coded::Code
                }
                None => Coded::Text,
            };
            out.push(Rendered {
                line,
                coded,
                units,
                prefix,
                cells,
            });
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
                    cells: Vec::new(),
                    prefix,
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
    /// The body, without its block prefix; `full_line` includes that prefix.
    pub line: Line<'static>,
    pub coded: Coded,
    pub units: crate::unit::Units,
    pub cells: Vec<crate::unit::CellFragment>,
    pub prefix: Option<Prefix>,
}

/// Equal-width first and continuation prefixes keep list markers on the first
/// screen row and quote rules on every screen row.
#[derive(Debug, Clone, PartialEq)]
pub struct Prefix {
    pub first: Line<'static>,
    pub rest: Line<'static>,
    /// A thematic break stretches to the available body width when drawn.
    pub rule: bool,
}

impl Prefix {
    /// A marker occupies its own column and leaves an equal-width continuation margin.
    pub(crate) fn hanging(first: Line<'static>) -> Self {
        Self {
            rest: Line::from(" ".repeat(first.width())),
            first,
            rule: false,
        }
    }

    pub fn fit_body(&self, mut line: Line<'static>, width: usize) -> Line<'static> {
        if self.rule {
            let room = width
                .checked_sub(self.first.width())
                .filter(|n| *n > 0)
                .unwrap_or(width);
            if let Some(span) = line.spans.first_mut() {
                span.content = "─".repeat(room).into();
            }
        }
        line
    }
}

impl Rendered {
    pub fn full_line(&self) -> Line<'static> {
        let mut spans = self
            .prefix
            .as_ref()
            .map_or_else(Vec::new, |p| p.first.spans.clone());
        spans.extend(self.line.spans.clone());
        Line::from(spans).style(self.line.style)
    }

    pub fn full_units(&self) -> crate::unit::Units {
        let lead = self.prefix.as_ref().map_or(0, |p| p.first.width());
        self.units
            .iter()
            .map(|r| r.start + lead..r.end + lead)
            .collect()
    }
}

/// Parse and render Markdown into styled lines, including block prefixes.
/// Styles ride on the spans; nothing here speaks SGR.
pub fn render_markdown(text: &str, paint: &Paint) -> Vec<ratatui::text::Line<'static>> {
    render_coded(text, paint)
        .into_iter()
        .map(|r| r.full_line())
        .collect()
}

/// The same lines, each with what it is to a code block and what in it
/// reads as one.
pub fn render_coded(text: &str, paint: &Paint) -> Vec<Rendered> {
    let width = crossterm::terminal::size()
        .ok()
        .map(|(w, _)| usize::from(w));
    render_coded_at(text, paint, width)
}

pub(crate) fn render_coded_at(text: &str, paint: &Paint, width: Option<usize>) -> Vec<Rendered> {
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
                cells: Vec::new(),
                prefix: None,
            })
            .collect();
    }
    let trimmed = trim_partial_fences(text);
    // Diagrams share the width budget of the surrounding Markdown.
    let drawn = crate::block::drawn(trimmed, width);
    let marked = crate::unit::mark(&drawn);
    let badge =
        style_to_ratatui(&paint.theme.code).patch(band_to_ratatui(&paint.theme.prompt.panel.said));
    let muted = style_to_ratatui(&paint.theme.muted);
    let options = paint.markdown.get_or_init(|| {
        crate::markdown::options(
            paint.theme.clone(),
            crate::theme::table_separator(paint.foreground, paint.background),
            paint.background,
        )
    });
    let parsed = options.render(&marked, width);
    // Flatten block and inline styles onto the owned spans before measuring.
    let mut table_units = Vec::with_capacity(parsed.len());
    let mut table_cells = Vec::with_capacity(parsed.len());
    let (lines, prefixes): (Vec<_>, Vec<_>) = parsed
        .into_iter()
        .map(|row| {
            table_units.push(row.units);
            table_cells.push(row.cells);
            let line = row.line;
            let line_style = line.style;
            let spans: Vec<Span<'static>> = line
                .spans
                .into_iter()
                .map(|span| {
                    let style = crate::markdown::inline(
                        line_style.patch(span.style),
                        &paint.theme,
                        paint.background,
                    );
                    Span::styled(span.content.into_owned(), style)
                })
                .collect();
            (Line::from(spans), row.prefix)
        })
        .unzip();
    let lines = match marked {
        Cow::Owned(_) => crate::unit::strip(lines),
        Cow::Borrowed(_) => lines.into_iter().map(|l| (l, Vec::new())).collect(),
    };
    let lines: Vec<_> = lines
        .into_iter()
        .zip(table_units)
        .zip(prefixes)
        .map(|(((line, mut units), table_units), prefix)| {
            units.extend(table_units);
            (line, units, prefix)
        })
        .collect();
    let lines = lines
        .into_iter()
        .zip(table_cells)
        .map(|((line, units, prefix), cells)| Rendered {
            line,
            units,
            prefix,
            cells,
            coded: Coded::Text,
        })
        .collect();
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

pub use agent::Outcome;

/// The mark a finished call's `outcome` says.
pub fn outcome_glyph(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Done | Outcome::Warned => icons::DONE_MARK,
        Outcome::Failed => icons::FAIL_MARK,
    }
}

/// The style that mark is drawn in.
pub fn outcome_style(outcome: Outcome, theme: &Theme) -> &Style {
    match outcome {
        Outcome::Done => &theme.status.ok,
        Outcome::Warned => &theme.status.warn,
        Outcome::Failed => &theme.status.err,
    }
}

/// A finished call, as its result row draws it.
pub struct Finished<'a> {
    pub outcome: Outcome,
    pub name: &'a str,
    pub preview: &'a str,
    /// Where it ran, when not the workspace root.
    pub place: Option<&'a str>,
    /// How long it ran, when known.
    pub took: Option<std::time::Duration>,
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
    result_parts(call, shown, hovered, p, width)
        .into_iter()
        .map(|(line, border)| {
            let mut spans = border.map_or_else(Vec::new, |b| b.spans);
            spans.extend(line.spans);
            Line::from(spans).style(line.style)
        })
        .collect()
}

pub(crate) const RESULT_INDENT: &str = "  ";

pub(crate) fn result_parts(
    call: &Finished,
    shown: Shown,
    hovered: bool,
    p: &Paint,
    width: usize,
) -> Vec<(Line<'static>, Option<Line<'static>>)> {
    let room = result_room(width);
    let (mark_style, mark) = (
        outcome_style(call.outcome, &p.theme),
        outcome_glyph(call.outcome),
    );
    let (head, rest) = call.preview.split_once('\n').unwrap_or((call.preview, ""));
    // Past the mark, the name and the `…` a cut adds; the place and time
    // are kept whole, the command gives way to them.
    let placed = placed(call.place) + &ran_for(call.took);
    let fits = title_room(room, Line::raw(format!("{}  ", call.name)).width(), &placed);
    let border = Line::from(vec![p.span(mark_style, mark), Span::raw(" ")]);
    let mut spans = p.tool_call(
        call.name,
        &format!("{} {}", call.name, clip(head, fits)),
        hovered,
    );
    spans.push(p.span_hovered(hovered, &p.theme.muted, placed));
    let mut out = vec![(Line::from(spans), Some(border))];
    let diff_lines: Vec<&str> = rest.lines().collect();
    let (rows, footer) = sketched(&diff_lines, shown);
    // A page reads the same rows a row does: the change keeps its colours, so
    // what the row showed small is the same thing read whole.
    let sketch = diff::content(rows, p, room, true);
    out.extend(
        sketch
            .into_iter()
            .map(|line| (line, Some(Line::from(RESULT_INDENT)))),
    );
    out.extend(footer.map(|text| {
        (
            Line::from(p.action_hint(hovered, text.trim_start())),
            Some(Line::from(RESULT_INDENT)),
        )
    }));
    out
}

/// What of the lines under a head `shown` lets through, and the footer
/// that says the rest is folded away or can be folded back.
pub fn sketched<T>(under: &[T], shown: Shown) -> (&[T], Option<String>) {
    match shown {
        Shown::Sketch if folds(1 + under.len()) => (
            &under[..SKETCH_LIMIT],
            Some(format!("  {} more", under.len() - SKETCH_LIMIT)),
        ),
        Shown::Open if folds(1 + under.len()) => (under, Some("  click for less".into())),
        _ => (under, None),
    }
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
    llm::figures::elapsed(std::time::Duration::from_millis(ms))
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
        Event::ToolQueued { name, args, .. } => {
            let mut spans = vec![p.span(&p.theme.muted, icons::PENDING_MARK), Span::raw(" ")];
            spans.extend(p.tool_call(name, &named(name, &summarize(args)), false));
            Line::from(spans)
        }
        Event::ToolEnd {
            name,
            outcome,
            preview,
            took,
            ..
        } => {
            let call = Finished {
                outcome: *outcome,
                name,
                preview,
                place: None,
                took: Some(*took),
            };
            return Some(result_rows(&call, Shown::Sketch, false, p, width));
        }
        Event::ToolDenied { name, reason, .. } => Line::from(vec![
            p.span(&p.theme.status.err, icons::FAIL_MARK),
            p.span(&p.theme.prompt.color, format!(" {name} ")),
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
    model_name: String,

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
        model_name: String,
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
            model_name,
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
                    eprint!(
                        "{}",
                        self.paint.on(&self.paint.reasoning_style(), "thinking ")
                    );
                    self.thinking = true;
                }
                eprint!("{}", self.paint.on(&self.paint.reasoning_style(), d));
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
                    .snapshot(&self.model_name, self.worktree.as_deref(), None, 0);
                let line = crate::status::line(&self.status, &snap);
                if !line.is_empty() {
                    let painted = crate::status::painted(&line, &self.paint);
                    eprintln!("{}", lines_to_ansi(&[painted]));
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
/// draws the prefix (spinner, queued mark, or finished mark) itself.
pub fn named(name: &str, summary: &str) -> String {
    if summary.is_empty() {
        name.to_string()
    } else {
        format!("{name} {summary}")
    }
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

/// How long a call runs before its line says so. A quick one's time is
/// nothing to read, and a `1s` that shows only to vanish reads as a flicker.
pub const TIMED_AFTER: std::time::Duration = std::time::Duration::from_secs(2);

/// A time as the tail of a line, ` · 5s`: how long a call or execution ran, or
/// has been out so far.
pub fn lasted(d: std::time::Duration) -> String {
    format!("{}{}", icons::PART_SEP, llm::figures::elapsed(d))
}

/// What a call's line says of its time: nothing under `TIMED_AFTER`.
pub fn ran_for(took: Option<std::time::Duration>) -> String {
    took.filter(|d| *d >= TIMED_AFTER)
        .map_or_else(String::new, lasted)
}

/// The one argument worth showing in a progress line: its first line, `…`
/// saying there is more of it.
pub fn summarize(args: &serde_json::Value) -> String {
    let v = tool::summary(args);
    let first = v.lines().next().unwrap_or_default();
    pi_store::text::clip_more(first, 80, first.len() < v.trim_end().len())
}

#[cfg(test)]
mod tests {
    use super::Paint;

    // What the printed form writes, `parse_sgr` reads back unchanged: every
    // colour form, in either slot, with every attribute.
    #[test]
    fn a_style_survives_the_round_trip_through_sgr() {
        use ratatui::style::{Color, Modifier, Style};
        let colors = crate::sgr::NAMED
            .into_iter()
            .chain(crate::sgr::BRIGHT)
            .chain([Color::Reset, Color::Indexed(208), Color::Rgb(88, 166, 255)]);
        for color in colors {
            for style in [
                Style::default().fg(color),
                Style::default().bg(color),
                Style::default().fg(color).bg(Color::Indexed(17)),
            ] {
                let mut params = String::new();
                super::append_style_params(&mut params, style);
                assert_eq!(
                    crate::sgr::parse_sgr(&params, Style::default()),
                    style,
                    "{params}"
                );
            }
        }
        let all = Style::default().add_modifier(Modifier::all());
        let mut params = String::new();
        super::append_style_params(&mut params, all);
        assert_eq!(
            crate::sgr::parse_sgr(&params, Style::default()),
            all,
            "{params}"
        );
    }

    #[test]
    fn a_tool_call_keeps_the_command_verbatim() {
        let text = "bash cd /home/wukaige/ai-agent && git diff --stat | cat; echo \"=== 改动\"; git diff src/server/app.py | head -40";
        for color in [true, false] {
            let paint = Paint::new(color);
            for command in [
                text,
                "bash printf 'unfinished",
                "bash echo 你好\n\tgit diff",
            ] {
                let spans = paint.tool_call("bash", command, false);
                let plain: String = spans.iter().map(|s| s.content.as_ref()).collect();
                assert_eq!(plain, command);
                assert!(spans.iter().all(|s| !s.content.contains('\x1b')));
            }
        }
    }

    #[test]
    fn a_tool_call_keeps_result_text_and_width() {
        let call = super::Finished {
            outcome: super::Outcome::Done,
            name: "bash",
            preview: "cd /home/wukaige/ai-agent && echo \"=== 改动\"; git diff src/server/app.py | head -40",
            place: Some("crates/ui"),
            took: Some(std::time::Duration::from_secs(3)),
        };
        for width in [24, 80, 180] {
            let plain = super::result_rows(
                &call,
                super::Shown::Sketch,
                false,
                &Paint::new(false),
                width,
            );
            let painted =
                super::result_rows(&call, super::Shown::Sketch, false, &Paint::new(true), width);
            assert_eq!(plain.len(), painted.len());
            for (plain, painted) in plain.iter().zip(&painted) {
                assert_eq!(plain.to_string(), painted.to_string());
                assert_eq!(plain.width(), painted.width());
            }
            assert!(painted[0].to_string().ends_with(" · in crates/ui · 3s"));
        }
    }

    // A `-` / `+` pair whose rows lead differently, or have no number to
    // lead them, is drawn row by row rather than cut where the other led.
    #[test]
    fn a_rewritten_pair_is_cut_where_each_row_leads() {
        let paint = Paint::new(false);
        for (was, is) in [
            ("-    let x = 1;", "+    let x=1;"),
            ("-  9 a", "+ 10 ab"),
            ("- 1 改", "+ 1"),
            ("-    9 x", "+ 1 éé"),
        ] {
            for room in [0, 3, 6, 40] {
                let rows = super::sketch_rows(&["test.rs +1 -1", was, is], &paint, room, true);
                // The block's head, then the pair itself.
                assert_eq!(rows.len(), 3);
            }
        }
        let rows = super::sketch_rows(&["-  9 a", "+ 10 ab"], &paint, 40, true);
        let text: Vec<String> = rows.iter().map(ToString::to_string).collect();
        assert_eq!(text, ["  -  9 a", "  + 10 ab"]);
    }

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
