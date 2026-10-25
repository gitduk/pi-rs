//! One row of scrollback, the only place one can be made. `Kind` is
//! private, so the two producers (live stream, rebuild) can't disagree.

use std::cell::RefCell;

use llm::message::ToolResult;
use ratatui::style::Style as RStyle;
use ratatui::text::{Line, Span};

use crate::block;
use crate::render::named;
use crate::render::{self, Paint};
use crate::tui::browse::PageLine;
use crate::tui::screen;
use pi_core::core::resolve::Resolved;
use pi_store::icons;

mod code;
mod note;
mod relayed;
mod result;
mod steps;

use code::{Block, Code};
use note::Note;
use relayed::Relayed;
use result::ToolOut;
use steps::Group;
pub use steps::{FoldedTool, PendingTool, Stage, Step, StepMark, out_for};

pub struct Row(Kind, Height);

// The rows this row takes on screen at one width, wraps included —
// counted per logical line and cached until width or content changes.
#[derive(Default)]
struct Height(RefCell<Option<Measured>>);

struct Measured {
    width: usize,
    // One entry per logical line, `None` until wrapped. Kept per line so
    // a tall row is never wrapped whole just to answer for one line.
    lines: Vec<Option<usize>>,
}

impl Height {
    // Measurement at this width, room for `lines` of them. A moved width
    // restarts; a grown row keeps what's counted (lines can't shrink).
    fn at(&self, width: usize, lines: usize) -> std::cell::RefMut<'_, Measured> {
        let mut held = self.0.borrow_mut();
        match held.as_mut() {
            Some(m) if m.width == width => {
                if m.lines.len() < lines {
                    m.lines.resize(lines, None);
                }
            }
            _ => {
                *held = Some(Measured {
                    width,
                    lines: vec![None; lines],
                });
            }
        }
        std::cell::RefMut::map(held, |h| h.as_mut().expect("filled above"))
    }

    fn line(&self, width: usize, i: usize) -> Option<usize> {
        self.0
            .borrow()
            .as_ref()
            .filter(|m| m.width == width)
            .and_then(|m| m.lines.get(i).copied().flatten())
    }

    fn set(&self, width: usize, i: usize, height: usize) {
        self.at(width, i + 1).lines[i] = Some(height);
    }

    fn clear(&self) {
        *self.0.borrow_mut() = None;
    }
}

enum Kind {
    // One logical line of a said prompt: border and body kept apart so
    // a wrap can repeat the border. `band` is the whole row's background.
    Said {
        // The rule and its column: `SAID_RULE` in the prompt colour.
        border: Line<'static>,
        // The line's text, in the input style, without the border.
        body: Line<'static>,
        band: Option<RStyle>,
    },
    // What the model answered, one row per markdown line, with what in it
    // reads as one. Its own kind, not a notice: it is half the conversation.
    Answer(Line<'static>, crate::unit::Units),
    // A line of an answer's code block, indented by `CODE_INDENT`.
    CodeLine(Line<'static>),
    Code(Code),
    Block(Block),
    // A painted line the screen alone knows: banner, command output,
    // warning. `times` collapses an identical repeat into one row+count.
    Notice {
        text: Line<'static>,
        times: usize,
        // Drawn only, never filed: what a tidy takes.
        aside: bool,
    },
    Note(Note),
    // Painted lines that come and go together: the banner, or none at all
    // where a row was that is gone.
    Lines(Vec<Line<'static>>),
    Result(ToolOut),
    Relayed(Relayed),
    Steps(Group),
}

/// What opening a closed line shows on the reading page: what it reads
/// opened in place, whole.
pub type Opening = Vec<PageLine>;

/// A closed line's lines as opening it in place draws them, whole, each with
/// the border a wrap repeats.
pub type Opened = Vec<(PageLine, Option<Line<'static>>)>;

// Opened lines as a page reads them: each border before its line, less the
// indent they all share, which only sat them under the row's own line.
fn page_of(lines: Opened) -> Opening {
    let indent = |border: &Option<Line<'static>>| {
        let lead = border.as_ref().map(screen::plain).unwrap_or_default();
        lead.len() - lead.trim_start_matches(' ').len()
    };
    let shared = lines.iter().map(|(_, b)| indent(b)).min().unwrap_or(0);
    (lines.into_iter())
        .map(|(mut page, border)| {
            let mut cut = shared;
            let mut spans = border.as_ref().map(screen::spans_of).unwrap_or_default();
            for span in &mut spans {
                let k = span.content.len().min(cut);
                span.content = span.content[k..].to_string().into();
                cut -= k;
            }
            spans.retain(|s| !s.content.is_empty());
            let lead: usize = spans
                .iter()
                .map(|s| pi_store::text::width(&s.content))
                .sum();
            if lead > 0 {
                spans.extend(page.line.spans);
                page.line = Line::from(spans);
                for unit in &mut page.units {
                    *unit = unit.start + lead..unit.end + lead;
                }
            }
            page
        })
        .collect()
}

// A row that opens and closes. A new kind that folds implements this and
// joins `Row::fold`; nothing else about folding names the kinds.
trait Fold {
    // Whether opening would show more than the row's own line.
    fn expandable(&self) -> bool;
    // Open if closed, close if open; the caller checked `expandable`.
    fn flip(&mut self);
    // The line a click on line `i` opens or closes: by default the row's
    // own, from anywhere on it.
    fn click_line(&self, _i: usize, _width: usize) -> Option<usize> {
        self.expandable().then_some(0)
    }
    // Open or close what line `i` heads; whether anything moved.
    fn toggle_at(&mut self, _i: usize, _width: usize) -> bool {
        self.expandable() && {
            self.flip();
            true
        }
    }
    // What opening line `i` in place would show while closed, whole since a
    // page reads it wrapped; drawn in place by the same code.
    fn opened(&self, i: usize, paint: &Paint) -> Option<Opened>;
    // What the page reads for line `i`: what `opened` gives, as a page.
    fn opening(&self, i: usize, paint: &Paint) -> Option<Opening> {
        self.opened(i, paint).map(page_of)
    }
    // Whether lines are cut at `width` that only its opening shows whole.
    fn clipped(&self, _width: usize) -> bool {
        false
    }
}

impl Row {
    // A row whose height is not measured yet; every constructor starts here.
    fn new(kind: Kind) -> Self {
        Self(kind, Height::default())
    }

    // The row as something that opens and closes, if it is one.
    fn fold(&self) -> Option<&dyn Fold> {
        match &self.0 {
            Kind::Steps(g) => Some(g),
            Kind::Result(r) => Some(r),
            Kind::Relayed(r) => Some(r),
            Kind::Note(n) => Some(n),
            _ => None,
        }
    }

    fn fold_mut(&mut self) -> Option<&mut dyn Fold> {
        match &mut self.0 {
            Kind::Steps(g) => Some(g),
            Kind::Result(r) => Some(r),
            Kind::Relayed(r) => Some(r),
            Kind::Note(n) => Some(n),
            _ => None,
        }
    }

    /// Something only the screen ever knew (a tally line, a lane's
    /// answer, a warning), in the surface's own muted voice.
    pub fn notice(line: impl Into<Line<'static>>) -> Self {
        Self::new(Kind::Notice {
            text: line.into(),
            times: 1,
            aside: false,
        })
    }

    /// A notice the transcript never gets: gone with a tidy, or a rebuild.
    pub fn aside(line: impl Into<Line<'static>>) -> Self {
        Self::new(Kind::Notice {
            text: line.into(),
            times: 1,
            aside: true,
        })
    }

    pub fn is_aside(&self) -> bool {
        matches!(self.0, Kind::Notice { aside: true, .. })
    }

    /// Painted lines kept as one row: replaced whole, they leave the rows
    /// under them where they were.
    pub fn lines(lines: Vec<Line<'static>>) -> Self {
        Self::new(Kind::Lines(lines))
    }

    /// The blank line a turn after the first opens on.
    pub fn gap() -> Self {
        Self::new(Kind::Answer(Line::default(), Vec::new()))
    }

    /// Whether this row is where a turn begins: something said, or relayed.
    pub fn opens_turn(&self) -> bool {
        matches!(&self.0, Kind::Said { .. } | Kind::Relayed(_))
    }

    /// A turn pi sent on the model's behalf, folded under `label`, come in
    /// at `at` (wall-clock seconds).
    pub fn relayed(label: &str, text: &str, at: u64, paint: &Paint) -> Self {
        Self::new(Kind::Relayed(Relayed::new(label, text, at, paint)))
    }

    /// Plain text as notice rows, one per line — shared by the live
    /// stream (which also files it) and a rebuild (from the archived text).
    pub fn notice_lines(text: &str, paint: &Paint) -> Vec<Self> {
        text.lines()
            .map(|l| Self::notice(Line::from(paint.span(&paint.theme.muted, l))))
            .collect()
    }

    /// A note — what the model is told, shown as it reads it — as one row,
    /// or none for a note with no lines.
    pub fn note_lines(text: &str) -> Vec<Self> {
        match text.lines().next() {
            Some(_) => vec![Self::new(Kind::Note(Note::new(text)))],
            None => Vec::new(),
        }
    }

    /// Fold a repeat into this row, if it is the same notice: the scrollback
    /// then shows `line ×2` where a second row would have gone.
    ///
    /// Compares the spans, which is what makes it safe: two notices that
    /// read alike but are styled differently are different rows, and a row of
    /// any other kind never folds.
    pub fn repeated(&mut self, line: &Line<'_>) -> bool {
        match &mut self.0 {
            // Only onto an aside: a count on a filed row would outlive a tidy.
            Kind::Notice {
                text,
                times,
                aside: true,
            } if *text == *line => {
                *times += 1;
                self.1.clear();
                true
            }
            _ => false,
        }
    }

    /// One reasoning line. A reasoning line lives inside its group, not
    /// beside it.
    pub fn reasoning_line(line: &str, paint: &Paint) -> Line<'static> {
        Line::from(paint.span(&paint.theme.muted, line))
    }

    /// A whole assistant text block, for a caller that has one. Its mermaid
    /// blocks are rows of their own, drawn at whatever width they get.
    pub fn answer(text: &str, paint: &Paint) -> Vec<Self> {
        let markdown = |text: &str| {
            render::render_coded(text, paint)
                .into_iter()
                .map(|r| match r.coded {
                    render::Coded::Badge(code) => Self::new(Kind::Code(Code {
                        badge: r.line,
                        code,
                        note: Default::default(),
                    })),
                    render::Coded::Code => Self::new(Kind::CodeLine(r.line)),
                    render::Coded::Text => Self::new(Kind::Answer(r.line, r.units)),
                })
        };
        if !paint.color {
            return markdown(text).collect();
        }
        let blank = Self::gap;
        let is_blank =
            |row: &Self| matches!(row, Self(Kind::Answer(l, _), _) if l.spans.is_empty());
        let mut rows: Vec<Self> = Vec::new();
        for piece in block::pieces(text) {
            match piece {
                block::Piece::Text(text) => rows.extend(markdown(text)),
                block::Piece::Block(form, source) => {
                    if rows.last().is_some_and(|r| !is_blank(r)) {
                        rows.push(blank());
                    }
                    let source = source.trim_end().to_string();
                    rows.push(Self::new(Kind::Block(Block::new(form, source))));
                    rows.push(blank());
                }
            }
        }
        if rows.last().is_some_and(is_blank) {
            rows.pop();
        }
        rows
    }

    /// A group whose first step is `first`, folded the way it is born.
    pub fn steps(first: Step, folded: bool) -> Self {
        Self::new(Kind::Steps(Group::new(first, folded)))
    }

    /// A lone block of reasoning, as a group of one.
    #[cfg(test)]
    pub fn thinking(block: u64, lines: Vec<Line<'static>>, folded: bool) -> Self {
        let step = Step::thinking(block, lines, false, &Paint::new(false));
        Self::steps(step, folded)
    }

    /// Add a step to this row if it is a group; the step comes back if not.
    pub fn join(&mut self, step: Step) -> Result<(), Box<Step>> {
        let Kind::Steps(g) = &mut self.0 else {
            return Err(Box::new(step));
        };
        g.join(step);
        self.1.clear();
        Ok(())
    }

    /// Whether this row is a group.
    pub fn is_steps(&self) -> bool {
        matches!(&self.0, Kind::Steps(_))
    }

    /// The indent reasoning block `id` shows its lines under, if this group
    /// shows them; the block's unfinished line follows them there.
    pub fn shows_block(&self, id: u64) -> Option<&'static str> {
        match &self.0 {
            Kind::Steps(g) => g.shows_block(id),
            _ => None,
        }
    }

    /// Whether this row is an expandable row.
    pub fn is_expandable(&self) -> bool {
        self.fold().is_some_and(|f| f.expandable())
    }

    /// Toggle the row's own line, returning true if toggled.
    #[cfg(test)]
    pub fn toggle_expand(&mut self) -> bool {
        self.toggle_at(0, 0)
    }

    /// The line a click on line `i` opens or closes, if any: a result from
    /// its head or footer, a group from its own line or a step's.
    pub fn click_line(&self, i: usize, width: usize) -> Option<usize> {
        match &self.0 {
            Kind::Code(_) | Kind::Block(_) => Some(0),
            _ => self.fold()?.click_line(i, width),
        }
    }

    /// Open or close what line `i` heads — see `click_line` — and say
    /// whether anything moved.
    pub fn toggle_at(&mut self, i: usize, width: usize) -> bool {
        let moved = self.fold_mut().is_some_and(|f| f.toggle_at(i, width));
        if moved {
            self.1.clear();
        }
        moved
    }

    /// What opening line `i` would show, while it is closed: the line as
    /// it reads with nothing under the pointer, then what it opens onto — a
    /// step's reasoning or call, a result whole, a relayed turn's text.
    pub fn opening(&self, i: usize, paint: &Paint) -> Option<Opening> {
        self.fold()?.opening(i, paint)
    }

    /// Whether lines are cut at `width` that only its opening shows whole.
    pub fn clipped(&self, width: usize) -> bool {
        self.fold().is_some_and(|f| f.clipped(width))
    }

    /// Brings a row up to date before the frame draws: hover line, spinner
    /// tick, and the calls in flight this row draws.
    pub fn update_live(&mut self, hovered: Option<usize>, spin: usize, held: &[PendingTool]) {
        // Hover bolds, and bold is no wider: only what else changed moves
        // a height.
        let reshaped = match &mut self.0 {
            Kind::Steps(g) => g.update(hovered, spin, held),
            Kind::Result(r) => {
                r.hover(hovered.is_some());
                false
            }
            Kind::Code(Code { note, .. }) | Kind::Block(Block { note, .. }) => {
                note.update(hovered.is_some())
            }
            Kind::Relayed(r) => {
                r.hover(hovered.is_some());
                false
            }
            Kind::Note(n) => {
                n.hover(hovered.is_some());
                false
            }
            _ => false,
        };
        if reshaped {
            self.1.clear();
        }
    }

    /// A tool result the screen already has in parts — the live path, which
    /// never holds a `ToolResult`.
    pub fn result(
        outcome: crate::render::Outcome,
        name: impl Into<String>,
        preview: impl Into<String>,
    ) -> Self {
        Self::new(Kind::Result(ToolOut::new(
            outcome,
            name.into(),
            preview.into(),
        )))
    }

    /// Where the call behind this result ran, when not the workspace root.
    pub fn at(mut self, place: Option<String>) -> Self {
        if let Kind::Result(r) = &mut self.0 {
            r.place = place;
        }
        self
    }

    /// How long the call behind this result ran.
    pub fn timed(mut self, took: Option<std::time::Duration>) -> Self {
        if let Kind::Result(r) = &mut self.0 {
            r.took = took;
        }
        self
    }

    /// How long the call behind this result ran, if known.
    pub fn took(&self) -> Option<std::time::Duration> {
        match &self.0 {
            Kind::Result(r) => r.took,
            _ => None,
        }
    }

    /// Where the call behind this result ran, if it said and it was not the
    /// workspace root.
    pub fn place(&self) -> Option<&str> {
        match &self.0 {
            Kind::Result(r) => r.place.as_deref(),
            _ => None,
        }
    }

    /// The row for a stored result: the tool's own sketch if it made one
    /// (`ToolResult` lacks it), else the content's first line.
    pub fn stored_result(
        r: &ToolResult,
        preview: Option<&str>,
        outcome: crate::render::Outcome,
        took: Option<std::time::Duration>,
    ) -> Self {
        let preview = tool::preview_of(preview, &r.content);
        Self::result(outcome, r.name.clone(), preview).timed(took)
    }

    /// Say `said` beside this row's first line, until `FLASH` has passed.
    pub fn say_copied(&mut self, said: String) {
        if let Kind::Code(Code { note, .. }) | Kind::Block(Block { note, .. }) = &mut self.0 {
            note.copied(said);
            self.1.clear();
        }
    }

    /// Whether a badge heads this row at `width`, for a note to stand beside.
    pub fn badged(&self, paint: &Paint, width: usize) -> bool {
        match &self.0 {
            Kind::Code(_) => true,
            Kind::Block(b) => b.badged(paint, width),
            _ => false,
        }
    }

    /// What a click on this row copies: a code block's text, or the source
    /// of a diagram or table.
    pub fn code(&self) -> Option<&str> {
        match &self.0 {
            Kind::Code(c) => Some(&c.code),
            Kind::Block(b) => Some(&b.source),
            _ => None,
        }
    }

    /// The tool's name if this row is a tool result.
    pub fn tool_name(&self) -> Option<&str> {
        match &self.0 {
            Kind::Result(r) => Some(&r.name),
            _ => None,
        }
    }

    /// The tool's preview if this row is a tool result.
    pub fn tool_preview(&self) -> Option<&str> {
        match &self.0 {
            Kind::Result(r) => Some(&r.preview),
            _ => None,
        }
    }

    /// How this tool result's call came out.
    pub fn outcome(&self) -> Option<crate::render::Outcome> {
        match &self.0 {
            Kind::Result(r) => Some(r.outcome),
            _ => None,
        }
    }

    /// The line a call that was never answered keeps: stopped before it landed.
    pub fn tool_start(name: &str, summary: &str, paint: &Paint) -> Self {
        Self::notice(Line::from(
            paint.span(&paint.theme.muted, tool_start_line(name, summary)),
        ))
    }

    // One logical line of a prompt the user said: the rule it wears, the text
    // under it, and the panel both sit in. The border lives apart from the body.
    fn said(border: Line<'static>, body: Line<'static>, band: Option<RStyle>) -> Self {
        Self::new(Kind::Said { border, body, band })
    }

    /// A prompt's lines as the stream echoed them: the input bar, on a
    /// band one step off where it was typed, so it reads as typed there.
    pub fn prompt(text: &str, paint: &Paint) -> Vec<Self> {
        let band = paint.band(&paint.theme.prompt.panel.said);
        if text.starts_with('!') {
            // A `!` is a command, not something said: the bang takes the
            // prompt's place, and the lines under it keep the plain indent.
            let bang = paint.span(&paint.theme.prompt.color, icons::bar(icons::BANG_SIGIL));
            // Measured, never assumed: the continuation rows indent under the
            // bang by the columns the bang really takes.
            let indent = Span::raw(" ".repeat(bang.width()));
            let mut rows = Vec::new();
            for (i, line) in text.lines().enumerate() {
                let (prefix, body) = if i == 0 {
                    (
                        bang.clone(),
                        line.strip_prefix('!').unwrap_or(line).trim_start(),
                    )
                } else {
                    (indent.clone(), line)
                };
                rows.push(Self::said(
                    Line::from(prefix),
                    Line::from(paint.span(&paint.theme.input, body)),
                    band,
                ));
            }
            return rows;
        }
        // Unbroken down every line said: the icon marks what is being typed,
        // and a landed line wearing it reads as another place to type.
        let border =
            Line::from(paint.span(&paint.theme.prompt.color, icons::bar(icons::SAID_RULE)));
        text.lines()
            .map(|line| {
                Self::said(
                    border.clone(),
                    Line::from(paint.span(&paint.theme.input, line)),
                    band,
                )
            })
            .collect()
    }

    /// How many logical lines the row is made of — the `i`s `line` answers.
    /// Wraps not counted; `line_height` gives a line's screen rows.
    pub fn len(&self) -> usize {
        match &self.0 {
            Kind::Answer(..)
            | Kind::CodeLine(_)
            | Kind::Code(_)
            | Kind::Block(_)
            | Kind::Notice { .. }
            | Kind::Said { .. } => 1,
            Kind::Lines(lines) => lines.len(),
            Kind::Result(r) => r.len(),
            Kind::Relayed(r) => r.len(),
            Kind::Note(n) => n.len(),
            Kind::Steps(g) => g.len(),
        }
    }

    /// Screen rows logical line `i` takes at `width`, cached by width — a
    /// line never shown is counted without being wrapped.
    pub fn line_height(&self, i: usize, paint: &Paint, width: usize) -> usize {
        if let Some(h) = self.1.line(width, i) {
            return h;
        }
        let (line, border) = self.line(i, paint, width);
        let h = super::screen::wrap_rows(border.as_ref(), &line, width);
        self.1.set(width, i, h);
        h
    }

    /// What the screen renders for row `i` at this width: text, plus the
    /// border continuation rows repeat (a said row only; others have none).
    pub fn line(
        &self,
        i: usize,
        paint: &Paint,
        width: usize,
    ) -> (Line<'static>, Option<Line<'static>>) {
        match &self.0 {
            Kind::Said { border, body, .. } => (body.clone(), Some(border.clone())),
            Kind::Answer(text, _) => (text.clone(), None),
            Kind::Lines(lines) => (lines[i].clone(), None),
            Kind::CodeLine(line) => (line.clone(), Some(Line::from(CODE_INDENT))),
            Kind::Note(n) => n.line(i, paint, width),
            Kind::Code(c) => (c.line(paint), None),
            Kind::Block(b) => (b.line(paint, width), None),
            Kind::Notice { text, times, .. } if *times == 1 => (text.clone(), None),
            Kind::Notice { text, times, .. } => {
                // The count wears the muted style whatever the line it trails,
                // so a repeated warning still reads as one warning.
                let mut spans = super::screen::spans_of(text);
                spans.push(paint.span(
                    &paint.theme.muted,
                    format!(" {}", pi_store::text::times(*times)),
                ));
                (Line::from(spans), None)
            }
            Kind::Result(r) => (r.line(i, paint, width), None),
            Kind::Relayed(r) => r.line(i, paint, width),
            Kind::Steps(g) => g.line(i, paint, width),
        }
    }

    /// Whether a wrap of this row shows its border on the first piece only,
    /// blanking it on the rest; the columns it takes stay the same.
    pub fn hangs(&self) -> bool {
        matches!(self.0, Kind::Note(_))
    }

    /// What in line `i` reads as one — a stretch of code, a link, a table's
    /// cell — as cell ranges of the line `line` gives, its border not counted.
    pub fn units(&self, i: usize, paint: &Paint, width: usize) -> crate::unit::Units {
        match &self.0 {
            Kind::Answer(_, units) => units.clone(),
            Kind::Relayed(r) => r.units(i).to_vec(),
            Kind::Steps(g) => g.units(i, paint),
            Kind::Block(b) => b.units(paint, width),
            _ => Vec::new(),
        }
    }

    /// The panel this row's screen rows sit in, if it sits in one: the colour
    /// goes behind the row whole, past the text's own end.
    pub fn band(&self) -> Option<RStyle> {
        match &self.0 {
            Kind::Said { band, .. } => *band,
            _ => None,
        }
    }

    /// Whether this row is the group holding reasoning block `id`.
    pub fn holds_block(&self, id: u64) -> bool {
        matches!(&self.0, Kind::Steps(g) if g.holds_block(id))
    }

    /// Whether this row is folded, and the handle to change it.
    pub fn folded(&self) -> Option<bool> {
        match &self.0 {
            Kind::Steps(g) => Some(g.folded),
            _ => None,
        }
    }

    pub fn set_folded(&mut self, to: bool) {
        if let Kind::Steps(g) = &mut self.0 {
            g.folded = to;
            self.1.clear();
        }
    }

    /// Block `id` ended: folds to its first line if the group is folded,
    /// stays open if not; goes if it never had a line.
    pub fn end_block(&mut self, id: u64, paint: &Paint) -> bool {
        let Kind::Steps(g) = &mut self.0 else {
            return false;
        };
        let emptied = g.end_block(id, paint);
        self.1.clear();
        emptied
    }

    /// Append a finished line to reasoning block `id`. A no-op anywhere else,
    /// which no caller can reach: the row is found by `holds_block`.
    pub fn push_line(&mut self, id: u64, line: Line<'static>) {
        if let Kind::Steps(g) = &mut self.0 {
            g.push_line(id, line);
            self.1.clear();
        }
    }

    /// What the screen opens with: what the prompt stands on, departures
    /// from the defaults, and what startup had to say. Built fresh, so a
    /// reload theme change repaints it.
    pub fn banner(resolved: &Resolved, paint: &Paint) -> Self {
        let mut facts = Vec::new();
        if let Some(system) = &resolved.system {
            facts.push(("system prompt", tilde(system)));
        }
        if !resolved.instructions.is_empty() {
            facts.push(("instructions", resolved.instructions.join(", ")));
        }
        // By topic, a handful at most: the paths are in `/status`.
        const TOPICS: usize = 5;
        let topics: Vec<&str> = resolved
            .memory
            .iter()
            .filter_map(|p| std::path::Path::new(p).file_stem()?.to_str())
            .collect();
        if !topics.is_empty() {
            let mut shown = topics[..topics.len().min(TOPICS)].join(", ");
            if topics.len() > TOPICS {
                shown.push_str(&format!(", +{}", topics.len() - TOPICS));
            }
            facts.push(("memory", shown));
        }
        if !resolved.mcp.is_empty() {
            facts.push(("mcp", resolved.mcp.join(", ")));
        }
        if resolved.ceiling != tool::Tier::Exec {
            facts.push(("tier", format!("{:?}", resolved.ceiling).to_lowercase()));
        }
        // What the project file sets is a fact like the rest; which file
        // said it is not worth a line of its own.
        facts.extend(
            resolved
                .project
                .iter()
                .map(|(key, value)| (key.as_str(), value.clone())),
        );
        let width = pi_store::text::widest(facts.iter().map(|(label, _)| *label));
        let muted = |line: &str| Line::from(paint.span(&paint.theme.muted, line));
        let fact = |label: &str, value: &str| {
            muted(&format!("{}  {value}", pi_store::text::pad(label, width)))
        };
        Self::lines(
            std::iter::once(muted(icons::VERSION_BANNER))
                .chain(facts.iter().map(|(label, value)| fact(label, value)))
                .chain((!resolved.notes.is_empty()).then(|| muted("")))
                .chain(resolved.notes.iter().map(|note| muted(note)))
                .collect(),
        )
    }
}

/// What sets a code block's lines off from the prose around them: a
/// border, so a wrapped line keeps it on every row.
pub const CODE_INDENT: &str = " ";

/// What a result says in text, its other parts left out.
fn tool_start_line(name: &str, summary: &str) -> String {
    format!("{} {}", icons::STOPPED_MARK, named(name, summary))
}

pub(super) use llm::figures::count;

// `text` cut to one row beside a `lead` wide mark and the `tail` after it.
fn clip_beside(text: &str, lead: usize, tail: &str, width: usize) -> String {
    let room = width.saturating_sub(lead + pi_store::text::width(tail) + 1);
    pi_store::text::clip(text, room.max(8))
}

// The home directory as `~`: the banner has no workspace to shorten against.
fn tilde(path: &std::path::Path) -> String {
    match pi_core::core::instructions::home()
        .and_then(|h| Some(path.strip_prefix(h).ok()?.to_owned()))
    {
        Some(rel) => format!("~/{}", rel.display()),
        None => path.display().to_string(),
    }
}

// A badge copies its block, as an answer's does; a block's line is
// indented as an answer's is, standing on its own.
impl From<crate::render::Rendered> for PageLine {
    fn from(r: crate::render::Rendered) -> Self {
        use crate::render::Coded;
        match r.coded {
            Coded::Badge(code) => Self {
                line: r.line,
                copies: Some(code),
                units: Vec::new(),
            },
            Coded::Code => {
                let mut spans = vec![Span::raw(CODE_INDENT)];
                spans.extend(r.line.spans);
                Line::from(spans).into()
            }
            Coded::Text => Self {
                line: r.line,
                copies: None,
                units: r.units,
            },
        }
    }
}

#[cfg(test)]
mod height_tests {
    use super::*;

    // The window counts a line by its height, wraps included, without
    // building the rows it skips.
    #[test]
    fn a_lines_height_counts_its_wraps() {
        let paint = Paint::new(false);
        let row = Row::lines(vec![Line::from("abcdef"), Line::from("gh")]);
        assert_eq!(row.line_height(0, &paint, 2), 3, "abcdef wraps to three");
        assert_eq!(row.line_height(1, &paint, 2), 1);
        assert_eq!(row.line_height(0, &paint, 80), 1);
    }

    // Closed, a note is its first line and a count of the rest; a click
    // there opens it whole.
    #[test]
    fn a_note_shows_its_first_line_until_opened() {
        let paint = Paint::new(false);
        let mut row = Row::note_lines("head\none\ntwo").remove(0);
        assert_eq!(row.len(), 1);
        let text = |row: &Row, i| row.line(i, &paint, 80).0.to_string();
        assert!(
            text(&row, 0).starts_with("head · 2 lines"),
            "{}",
            text(&row, 0)
        );
        assert_eq!(row.click_line(0, 80), Some(0));
        assert!(row.toggle_at(0, 80));
        assert_eq!(row.len(), 3);
        assert_eq!(text(&row, 0), "head");
        assert_eq!(text(&row, 2), "two");
        assert_eq!(row.click_line(1, 80), None);

        let lone = Row::note_lines("just this").remove(0);
        assert_eq!(text(&lone, 0), "just this");
        assert_eq!(lone.click_line(0, 80), None);

        let long = Row::note_lines(&"word ".repeat(40)).remove(0);
        assert_eq!(long.click_line(0, 40), Some(0), "cut, so it opens");
        let page = long.opening(0, &paint).unwrap();
        assert_eq!(page.len(), 1);
        let head = page[0].line.to_string();
        assert!(head.ends_with("word "), "uncut: {head}");
    }
}
