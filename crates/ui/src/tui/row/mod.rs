//! One row of scrollback, the only place one can be made. `Kind` is
//! private, so the two producers (live stream, rebuild) can't disagree.

use std::cell::RefCell;

use llm::message::{ToolResult, ToolResultContent};
use ratatui::style::Style as RStyle;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::block;
use crate::render::named;
use crate::render::{self, Paint};
use crate::tui::browse::PageLine;
use pi_core::core::resolve::Resolved;
use pi_store::icons;

mod code;
mod relayed;
mod result;
mod steps;

use code::{Block, Code};
use relayed::Relayed;
use result::ToolOut;
use steps::Group;
pub use steps::{FoldedTool, PendingTool, Step, call_frame, out_for};

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
    // What the model answered, one row per markdown line. Its own kind,
    // not a notice — an answer is half the conversation, not screen-only.
    Answer(Line<'static>),
    // A line of an answer's code block, indented by `CODE_INDENT`.
    CodeLine(Line<'static>),
    Code(Code),
    Block(Block),
    // A painted line the screen alone knows: banner, command output,
    // warning. `times` collapses an identical repeat into one row+count.
    Notice {
        text: Line<'static>,
        times: usize,
    },
    // Painted lines that come and go together: the banner, or none at all
    // where a row was that is gone.
    Lines(Vec<Line<'static>>),
    Result(ToolOut),
    Relayed(Relayed),
    Steps(Group),
}

/// What opening a closed line shows on the reading page: the line as it
/// reads unlit, then what it opens onto.
pub type Opening = (Line<'static>, Vec<PageLine>);

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
    // What opening line `i` would show, while it is closed.
    fn opening(&self, i: usize, paint: &Paint, width: usize) -> Option<Opening>;
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
            _ => None,
        }
    }

    fn fold_mut(&mut self) -> Option<&mut dyn Fold> {
        match &mut self.0 {
            Kind::Steps(g) => Some(g),
            Kind::Result(r) => Some(r),
            Kind::Relayed(r) => Some(r),
            _ => None,
        }
    }

    /// Whether this row is part of the conversation: what was said and what
    /// was answered, which is what browse mode is for.
    pub fn is_conversation(&self) -> bool {
        matches!(
            &self.0,
            Kind::Said { .. }
                | Kind::Answer(_)
                | Kind::CodeLine(_)
                | Kind::Code(_)
                | Kind::Block(_)
                | Kind::Relayed(_)
        )
    }

    /// Something only the screen ever knew (a tally line, a lane's
    /// answer, a warning), in the surface's own muted voice.
    pub fn notice(line: impl Into<Line<'static>>) -> Self {
        Self::new(Kind::Notice {
            text: line.into(),
            times: 1,
        })
    }

    /// Painted lines kept as one row: replaced whole, they leave the rows
    /// under them where they were.
    pub fn lines(lines: Vec<Line<'static>>) -> Self {
        Self::new(Kind::Lines(lines))
    }

    /// The blank line a turn after the first opens on.
    pub fn gap() -> Self {
        Self::new(Kind::Answer(Line::default()))
    }

    /// Whether this row is where a turn begins: something said, or relayed.
    pub fn opens_turn(&self) -> bool {
        matches!(&self.0, Kind::Said { .. } | Kind::Relayed(_))
    }

    /// A turn pi sent on the model's behalf, folded under `label`.
    pub fn relayed(label: &str, text: &str, paint: &Paint) -> Self {
        Self::new(Kind::Relayed(Relayed::new(label, text, paint)))
    }

    /// Plain text as notice rows, one per line — shared by the live
    /// stream (which also files it) and a rebuild (from the archived text).
    pub fn notice_lines(text: &str, paint: &Paint) -> Vec<Self> {
        text.lines()
            .map(|l| Self::notice(Line::from(paint.span(&paint.theme.muted, l))))
            .collect()
    }

    /// Fold a repeat into this row, if it is the same notice: the scrollback
    /// then shows `line ×2` where a second row would have gone.
    ///
    /// Compares the spans, which is what makes it safe: two notices that
    /// read alike but are styled differently are different rows, and a row of
    /// any other kind never folds.
    pub fn repeated(&mut self, line: &Line<'_>) -> bool {
        match &mut self.0 {
            Kind::Notice { text, times } if *text == *line => {
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
                .map(|(line, coded)| match coded {
                    render::Coded::Badge(code) => Self::new(Kind::Code(Code {
                        badge: line,
                        code,
                        note: Default::default(),
                    })),
                    render::Coded::Code => Self::new(Kind::CodeLine(line)),
                    render::Coded::Text => Self::new(Kind::Answer(line)),
                })
        };
        if !paint.color {
            return markdown(text).collect();
        }
        let blank = || Self::new(Kind::Answer(Line::default()));
        let is_blank = |row: &Self| matches!(row, Self(Kind::Answer(l), _) if l.spans.is_empty());
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
        Self::steps(
            Step::Thinking {
                block,
                lines,
                open: false,
                live: false,
            },
            folded,
        )
    }

    /// Add a step to this row if it is a group; the step comes back if not.
    pub fn join(&mut self, step: Step) -> Result<(), Step> {
        let Kind::Steps(g) = &mut self.0 else {
            return Err(step);
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
    pub fn opening(&self, i: usize, paint: &Paint, width: usize) -> Option<Opening> {
        self.fold()?.opening(i, paint, width)
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
            _ => false,
        };
        if reshaped {
            self.1.clear();
        }
    }

    /// A tool result the screen already has in parts — the live path, which
    /// never holds a `ToolResult`.
    pub fn result(ok: bool, name: impl Into<String>, preview: impl Into<String>) -> Self {
        Self::new(Kind::Result(ToolOut::new(ok, name.into(), preview.into())))
    }

    /// The row for a stored result: the tool's own sketch if it made one
    /// (`ToolResult` lacks it), else the content's first line.
    pub fn stored_result(r: &ToolResult, preview: Option<&str>) -> Self {
        let preview = preview.map(str::to_string).unwrap_or_else(|| {
            let body = result_text(r);
            match body.split_once('\n') {
                Some((h, _)) => h.to_string(),
                None => body,
            }
        });
        Self::result(!r.is_error, r.name.clone(), preview)
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

    /// Whether this tool result succeeded.
    pub fn ok(&self) -> Option<bool> {
        match &self.0 {
            Kind::Result(r) => Some(r.ok),
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
            Kind::Answer(_)
            | Kind::CodeLine(_)
            | Kind::Code(_)
            | Kind::Block(_)
            | Kind::Notice { .. }
            | Kind::Said { .. } => 1,
            Kind::Lines(lines) => lines.len(),
            Kind::Result(r) => r.len(),
            Kind::Relayed(r) => r.len(),
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
        let h = super::screen::wrap(border.as_ref(), &line, width).len();
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
            Kind::Answer(text) => (text.clone(), None),
            Kind::Lines(lines) => (lines[i].clone(), None),
            Kind::CodeLine(line) => (line.clone(), Some(Line::from(CODE_INDENT))),
            Kind::Code(c) => (c.line(paint), None),
            Kind::Block(b) => (b.line(paint, width), None),
            Kind::Notice { text, times } if *times == 1 => (text.clone(), None),
            Kind::Notice { text, times } => {
                // The count wears the muted style whatever the line it trails,
                // so a repeated warning still reads as one warning.
                let mut spans = super::screen::spans_of(text);
                spans.push(paint.span(&paint.theme.muted, format!(" ×{times}")));
                (Line::from(spans), None)
            }
            Kind::Result(r) => (r.line(i, paint, width), None),
            Kind::Relayed(r) => r.line(i, paint, width),
            Kind::Steps(g) => g.line(i, paint, width),
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
    pub fn end_block(&mut self, id: u64) -> bool {
        let Kind::Steps(g) = &mut self.0 else {
            return false;
        };
        let emptied = g.end_block(id);
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
        let width = facts
            .iter()
            .map(|(label, _)| label.len())
            .max()
            .unwrap_or(0);
        let muted = |line: &str| Line::from(paint.span(&paint.theme.muted, line));
        let fact = |label: &str, value: &str| muted(&format!("{label:width$}  {value}"));
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
pub fn result_text(r: &ToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| match c {
            ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_start_line(name: &str, summary: &str) -> String {
    format!("{} {}", icons::STOPPED_MARK, named(name, summary))
}

/// "N thing(s)": the counts a line ends with.
pub(super) fn count(n: usize, thing: &str) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("{n} {thing}{s}")
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

fn clip_to(s: &str, max_cols: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > max_cols {
            return s[..i].trim_end();
        }
        used += w;
    }
    s.trim_end()
}

// `text` in `room` columns, closed by `…` when any of it was cut or `more`
// follows it: the mark sits on the text, never apart from it.
fn clipped(text: &str, room: usize, more: bool) -> String {
    if !more && UnicodeWidthStr::width(text) <= room {
        return text.to_string();
    }
    format!(
        "{}{}",
        clip_to(text, room.saturating_sub(1)),
        icons::ELLIPSIS
    )
}

#[cfg(test)]
mod conversation_tests {
    use super::*;

    // Browse mode is the whole reason the answer has a kind of its own:
    // it shows said/answered, nothing the conversation passed through.
    #[test]
    fn only_what_was_said_and_what_was_answered_is_the_conversation() {
        let paint = Paint::new(false);
        for row in Row::prompt("what is this", &paint)
            .into_iter()
            .chain(Row::answer("# it is this\nand that", &paint))
        {
            assert!(row.is_conversation());
        }
        let tool = Step::Tool(FoldedTool::new("read", "src/main.rs", "", ""));
        for row in [
            Row::notice("a command printed this"),
            Row::thinking(1, vec![Line::from("thinking")], false),
            Row::result(true, "read", "src/main.rs"),
            Row::steps(tool, true),
        ] {
            assert!(!row.is_conversation());
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
        let row = Row::thinking(1, vec![Line::from("abcdef"), Line::from("gh")], false);
        assert_eq!(row.line_height(0, &paint, 2), 3, "abcdef wraps to three");
        assert_eq!(row.line_height(1, &paint, 2), 1);
        assert_eq!(row.line_height(0, &paint, 80), 1);
    }
}

// A code block's line indented as an answer's is, standing on its own.
fn set_off(line: Line<'static>, code: bool) -> Line<'static> {
    if !code {
        return line;
    }
    let mut spans = vec![Span::raw(CODE_INDENT)];
    spans.extend(line.spans);
    Line::from(spans)
}
