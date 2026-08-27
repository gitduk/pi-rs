//! The interactive surface: one owner of the terminal for the whole session.
//!
//! The line-editing library that used to sit here owned the terminal only while
//! it was reading a line, which is what made a key press during a run
//! unreachable and left the renderer writing into a terminal nobody was
//! managing. Here a single loop holds raw mode from start to finish and
//! services three sources at once — the agent's events, the keyboard, and a
//! timer for the spinner — so nothing has to be bolted on beside it.

mod editor;
mod screen;
mod status;

use std::borrow::Cow;
use std::collections::HashSet;
use std::time::Instant;

use agent::session::EntryId;
use agent::{AgentError, Event, Totals};
use brain::message::{
    AssistantContent, Message, ReasoningContent, ToolCallId, ToolResult, ToolResultContent,
    UserContent,
};
use anyhow::Result;
use brain::stream::Usage;
use crossterm::event::{Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::keys::{Action, Keys, Press};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style as RStyle;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};
use crate::render::Style as ThemeStyle;
use crate::render::{self, Markdown, Paint};
use crate::repl::{self, Candidate, Choice, Command, Repl, Step};
use editor::Editor;
use screen::{Rows, Screen};
use std::sync::Arc;

/// What a folded run shows instead of what it is thinking.
const THINKING: &str = "thinking...";

const BANNER: &str = concat!("π ", env!("CARGO_PKG_VERSION"));

/// How close two Ctrl-C presses must be to read as one deliberate quit.
///
/// Borrowed from pi, which uses the same 500ms. A latching flag looks simpler
/// and is wrong: clear one half-typed line, type another, clear that — and the
/// second clear reads as the second half of a double-tap and quits.
const DOUBLE_TAP: std::time::Duration = std::time::Duration::from_millis(500);

/// Whether a press lands inside the double-tap window of the previous one,
/// and records the press either way.
fn double_tap(last: &mut Option<Instant>, now: Instant) -> bool {
    let hit = last.is_some_and(|p| now.duration_since(p) < DOUBLE_TAP);
    *last = Some(now);
    hit
}

/// What a key press asked the loop to do. Every press redraws regardless.
#[derive(Debug, PartialEq, Eq)]
enum Act {
    None,
    Submit(String),
    Interrupt,
    /// The rewind selector wants the session's user messages.
    OpenRewind,
    /// A message chosen from the rewind selector: the conversation rewinds
    /// there, that message kept and everything after it forgotten.
    Rewind(EntryId),
    Quit,
}
/// A line in the scrollback.
enum Entry {
    /// A plain, painted line.
    Plain(String),
    /// A block of reasoning that can be folded or unfolded.
    Folded {
        /// Which reasoning block this entry belongs to; the stream appends
        /// completed lines to the open block's entry and nothing else.
        block: u64,
        /// The actual reasoning lines, kept so they can be shown when unfolded.
        lines: Vec<String>,
        /// Whether the block is currently folded.
        folded: bool,
    },
}
/// Whether reasoning is folded to its count line, and which block the stream
/// is filling right now.
///
/// Thinking always lives in a foldable scrollback entry, folded or not: the
/// screen is repainted from its entries every frame, so a line already shown
/// can still be folded. A block's own state lasts only while it is last; the
/// next block pushes it back to `folded`, the switch.
struct Thinking {
    /// The next block id; closed entries keep the id they were born with, so
    /// `land` appends only to the open block's entry.
    next: u64,
    /// The block streaming right now, if any.
    streaming: Option<u64>,
    /// What untouched blocks are folded to: the value a block that stops being
    /// last folds back to, and the target a global flip is measured from.
    folded: bool,
    /// How the last block — the one `ctrl+t` names, finished or streaming —
    /// is folded. It survives the block itself, so the next block is born
    /// with it until the key flips it again.
    last: bool,
}

/// Shut: the reasoning is worth a glance while it runs and almost never worth
/// the scrollback it costs afterwards.
///
/// The only constructor, because a derived one would answer `false` here — the
/// opposite of what the type says two lines up, in the one place nobody would
/// think to look.
impl Default for Thinking {
    fn default() -> Self {
        Self {
            next: 1,
            streaming: None,
            folded: true,
            last: true,
        }
    }
}

impl Thinking {
    /// Whether a reasoning row is hidden behind the count line: dim, and the
    /// streaming block folded — its own entry when it has one, the last
    /// value it will be born with otherwise.
    fn holds(&self, dim: bool, above: &[Entry]) -> bool {
        dim && self.stream_fold(above)
    }

    /// How the block streaming now is folded: its entry's own state, or —
    /// before its first line lands — the last value.
    fn stream_fold(&self, above: &[Entry]) -> bool {
        if let Some(id) = self.streaming
            && let Some(Entry::Folded { folded, .. }) = above
                .iter()
                .rev()
                .find(|e| matches!(e, Entry::Folded { block, .. } if *block == id))
        {
            return *folded;
        }
        self.last
    }

    /// The block that was last stops being so: it folds back to the switch.
    /// A new input and a new block both push it out of last.
    fn retire_last(&mut self, above: &mut [Entry]) {
        if let Some(Entry::Folded { folded, .. }) = last_folded(above) {
            *folded = self.folded;
        }
    }

    /// A new reasoning block is about to start. It gets an id the scrollback
    /// entry will be born with; its first line takes `birth_fold`.
    fn start(&mut self, above: &mut [Entry]) {
        self.retire_last(above);
        self.streaming = Some(self.next);
        self.next += 1;
    }

    /// The block that was last stops being so the moment a new input is
    /// submitted: it folds to the switch, its unfold lasting only while it
    /// was last.
    fn fold_previous(&mut self, above: &mut [Entry]) {
        self.retire_last(above);
    }

    /// The streaming block is over; the entry it filled stays where it is.
    fn close_block(&mut self) {
        self.streaming = None;
    }

    /// The value the next block's entry is born with: however `ctrl+t` last
    /// left the last block.
    fn birth_fold(&self) -> bool {
        self.last
    }

    /// Fold or unfold every block in the scrollback and move the switch with
    /// them, the last block included: entries and switch must never disagree,
    /// or the next block is born with a stale value and a mixed screen can
    /// never fold back to a single state.
    fn flip_all(&mut self, above: &mut [Entry]) {
        self.folded = !self.folded;
        self.last = self.folded;
        for entry in above.iter_mut() {
            if let Entry::Folded { folded, .. } = entry {
                *folded = self.folded;
            }
        }
    }

    /// Flip the last block only: the one streaming, or the newest finished
    /// one when nothing is. The switch is left alone, so the blocks no one is
    /// touching keep what they had; a block with no entry yet is born with
    /// the flip.
    fn toggle_current(&mut self, above: &mut [Entry]) {
        self.last = !self.last;
        let flipped = if let Some(id) = self.streaming {
            above
                .iter_mut()
                .rev()
                .find(|e| matches!(e, Entry::Folded { block, .. } if *block == id))
        } else {
            last_folded(above)
        };
        if let Some(Entry::Folded { folded, .. }) = flipped {
            *folded = self.last;
        }
    }
}

/// The newest reasoning block's entry in the scrollback, if any.
fn last_folded(above: &mut [Entry]) -> Option<&mut Entry> {
    above
        .iter_mut()
        .rev()
        .find(|e| matches!(e, Entry::Folded { .. }))
}

/// The count line a shut thinking block leaves in the scrollback.
fn thinking_summary(n: usize) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("thinking · {n} line{s}")
}

/// The rows one scrollback entry renders to, and the row at index `i` of
/// them: a plain line is itself; a folded block is its summary, or its full
/// lines when unfolded.
fn entry_len(entry: &Entry) -> usize {
    match entry {
        Entry::Plain(_) => 1,
        Entry::Folded { lines, folded, .. } => {
            if *folded {
                1
            } else {
                lines.len()
            }
        }
    }
}

fn entry_row<'a>(entry: &'a Entry, i: usize, paint: &Paint) -> Cow<'a, str> {
    match entry {
        Entry::Plain(s) => Cow::Borrowed(s),
        Entry::Folded { lines, folded, .. } => {
            if *folded {
                // The count row is synthesized at draw time, so it takes its
                // muted styling here rather than from a painted entry.
                Cow::Owned(paint.on(&paint.theme.muted, &thinking_summary(lines.len())))
            } else {
                Cow::Borrowed(&lines[i])
            }
        }
    }
}

/// The scrollback as rows, walked from either end without flattening the
/// whole history: `window` only ever needs the newest `want` rows, and an
/// unfolded thinking block is not worth re-materializing per frame.
struct ScrollbackRows<'a> {
    entries: &'a [Entry],
    /// For the folded summary row, which is synthesized at draw time and so
    /// carries no paint of its own.
    paint: &'a Paint,
    /// Next entry to read from the front, and the row offset inside it.
    front: (usize, usize),
    /// Next entry to read from the back, and the row offset inside it.
    back: (usize, usize),
}

impl<'a> ScrollbackRows<'a> {
    fn new(entries: &'a [Entry], paint: &'a Paint) -> Self {
        let back = entries.len().saturating_sub(1);
        let back_row = if entries.is_empty() {
            0
        } else {
            entry_len(&entries[back])
        };
        Self {
            entries,
            paint,
            front: (0, 0),
            back: (back, back_row),
        }
    }
}

impl<'a> Iterator for ScrollbackRows<'a> {
    type Item = Cow<'a, str>;

    fn next(&mut self) -> Option<Cow<'a, str>> {
        // `next_back` guards the empty case through its row pointers; the
        // front walk would index `entries[0]` before any check.
        if self.entries.is_empty() {
            return None;
        }
        while self.front.0 <= self.back.0 {
            let entry = &self.entries[self.front.0];
            if self.front.0 == self.back.0 {
                if self.front.1 >= self.back.1 {
                    return None;
                }
                let row = entry_row(entry, self.front.1, self.paint);
                self.front.1 += 1;
                return Some(row);
            }
            if self.front.1 < entry_len(entry) {
                let row = entry_row(entry, self.front.1, self.paint);
                self.front.1 += 1;
                return Some(row);
            }
            self.front = (self.front.0 + 1, 0);
        }
        None
    }
}

impl<'a> DoubleEndedIterator for ScrollbackRows<'a> {
    fn next_back(&mut self) -> Option<Cow<'a, str>> {
        while self.front.0 <= self.back.0 {
            let entry = &self.entries[self.back.0];
            if self.front.0 == self.back.0 {
                if self.front.1 >= self.back.1 {
                    return None;
                }
                self.back.1 -= 1;
                return Some(entry_row(entry, self.back.1, self.paint));
            }
            if self.back.1 > 0 {
                self.back.1 -= 1;
                return Some(entry_row(entry, self.back.1, self.paint));
            }
            self.back = (self.back.0 - 1, entry_len(&self.entries[self.back.0 - 1]));
        }
        None
    }
}


/// The rows between the scrollback and the status line: the reasoning window,
/// and the paragraph still being written.
///
/// A free function because it is where both of this feature's bugs lived and
/// `Ui` cannot be built without a terminal — a decision no test can reach is
/// one that gets its second chance in front of the user.
fn body(
    thinking: &Thinking,
    above: &[Entry],
    md: &Markdown,
    dim: bool,
    open: &str,
    space: (usize, usize),
    paint: &Paint,
) -> Vec<String> {
    let (width, room) = space;
    if thinking.holds(dim, above) {
        // The block's count row in the scrollback already answers the fold
        // switch; the live placeholder is only for the moment before the
        // block's first completed line exists to count.
        let counted = thinking.streaming.is_some_and(|id| {
            above
                .iter()
                .rev()
                .any(|e| matches!(e, Entry::Folded { block, .. } if *block == id))
        });
        if !counted {
            return vec![paint.on(&paint.theme.muted, THINKING)];
        }
        return Vec::new();
    }
    if open.is_empty() {
        return Vec::new();
    }
    let painted = if dim {
        paint.on(&paint.theme.muted, open)
    } else {
        md.line(open, paint)
    };
    let mut rows = screen::fit(&painted, width);
    // A paragraph can outgrow the screen; the tail is the part still being
    // written, and the rest reaches scrollback when it closes.
    if rows.len() > room {
        rows.drain(..rows.len() - room);
    }
    rows
}

/// A tool call still running, shown as one animated row in the live region
/// until its result lands and the row scrolls up as a check or cross.
struct RunTool {
    id: String,
    name: String,
    summary: String,
}

/// The one row a still-running tool occupies. The frame is the animation;
/// `ToolEnd` and `abandon_tools` replace the row with a final line.
fn tool_row(frame: usize, name: &str, summary: &str) -> String {
    let arg = if summary.is_empty() {
        String::new()
    } else {
        format!(" {summary}")
    };
    format!("{} {name}{arg}", status::FRAMES[frame % status::FRAMES.len()])
}

/// One tool result as the ✓/✗ row the live stream would have scrolled up.
fn tool_result_line(r: &ToolResult, paint: &Paint, width: usize) -> String {
    let mark = if r.is_error {
        paint.on(&paint.theme.status.err, "✗")
    } else {
        paint.on(&paint.theme.status.ok, "✓")
    };
    let body: String = r
        .content
        .iter()
        .filter_map(|c| match c {
            ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    let head = body.split_once('\n').map_or(body.as_str(), |(h, _)| h);
    let room = width.saturating_sub(2).max(20);
    format!("{mark} {} {}", r.name, paint.on(&paint.theme.muted, &render::clip(head, room)))
}

/// A tool's start line: what an unanswered call keeps in the view.
fn tool_start_line(name: &str, summary: &str) -> String {
    let arg = if summary.is_empty() {
        String::new()
    } else {
        format!(" {summary}")
    };
    format!("→ {name}{arg}")
}

/// A prompt's lines as the stream echoed them: the first with its gutter,
/// the rest indented.
fn push_prompt_lines(out: &mut Vec<Entry>, text: &str, prompt: &str, bang_prompt: &str) {
    for (i, line) in text.lines().enumerate() {
        let (gutter, body) = if i == 0 {
            match line.strip_prefix('!') {
                Some(rest) => (bang_prompt, rest.trim_start()),
                None => (prompt, line),
            }
        } else {
            ("  ", line)
        };
        out.push(Entry::Plain(format!("{gutter}{body}")));
    }
}

/// The transcript as rows, exactly as the live stream would have drawn them:
/// prompts with their gutter, answers as markdown, tool calls as their result
/// lines, reasoning as a foldable block. A rewind rebuilds the screen from
/// this, so the view returns to the point the conversation did.
fn render_log(
    session: &agent::session::Session,
    paint: &Paint,
    prompt: &str,
    bang_prompt: &str,
    width: usize,
    folded: bool,
) -> Vec<Entry> {
    // A call whose result is in the session shows only its result row; one that
    // never got an answer (an interrupted turn) shows the start line instead,
    // the way `abandon_tools` leaves it.
    let answered: HashSet<ToolCallId> = session
        .live()
        .iter()
        .filter_map(|(_, m)| match m {
            Message::User { content } => Some(content),
            _ => None,
        })
        .flat_map(|content| {
            content.iter().filter_map(|c| match c {
                UserContent::ToolResult(r) => Some(r.call.clone()),
                _ => None,
            })
        })
        .collect();

    let amendments = session.amendments();
    let mut out = Vec::new();
    for (id, m) in session.live() {
        match m {
            Message::User { content } => {
                for c in content {
                    match c {
                        UserContent::Text(t) => {
                            push_prompt_lines(&mut out, &t.text, prompt, bang_prompt);
                        }
                        UserContent::ToolResult(r) => {
                            out.push(Entry::Plain(tool_result_line(r, paint, width)));
                        }
                        UserContent::Image(_) => {}
                    }
                }
                // A prompt folded into a tool-results message by
                // `append_user` is an amendment, not content; it echoes too.
                if let Some(parts) = amendments.get(&id) {
                    for part in parts {
                        push_prompt_lines(&mut out, part, prompt, bang_prompt);
                    }
                }
            }
            Message::Assistant { content, .. } => {
                for b in content {
                    match b {
                        AssistantContent::Text(t) => {
                            // A fresh instance per block, matching the live
                            // stream resetting its markdown at each close.
                            let mut md = Markdown::default();
                            for line in t.text.lines() {
                                let painted = md.line(line, paint);
                                md.advance(line);
                                out.push(Entry::Plain(painted));
                            }
                        }
                        AssistantContent::ToolCall(c) => {
                            if answered.contains(&c.id) {
                                continue;
                            }
                            out.push(Entry::Plain(
                                paint.on(
                                    &paint.theme.muted,
                                    &tool_start_line(&c.name, &render::summarize(&c.args)),
                                ),
                            ));
                        }
                        AssistantContent::Reasoning(r) => {
                            // Muted, exactly as the live stream paints a
                            // reasoning row: a rebuilt block must not come
                            // out brighter than the one it replaces.
                            let lines: Vec<String> = r
                                .content
                                .iter()
                                .filter_map(|c| match c {
                                    ReasoningContent::Text { text, .. } => Some(text.as_str()),
                                    _ => None,
                                })
                                .flat_map(|s| s.lines().map(str::to_string))
                                .map(|s| paint.on(&paint.theme.muted, &s))
                                .collect();
                            out.push(Entry::Folded {
                                // A rebuilt entry is never appended to, so the
                                // id only has to stay out of the live stream's.
                                block: 0,
                                lines,
                                folded,
                            });
                        }
                    }
                }
            }
            Message::System { .. } => {}
        }
    }
    out
}

/// Everything the terminal shows, and nothing the session knows.
struct Ui {
    screen: Screen,
    keys: Arc<Keys>,
    editor: Editor,
    paint: Paint,
    /// The painted prompt gutter, shared by the editor and the echoed lines.
    prompt: String,
    /// The same gutter for a `!` line, where the bang takes the icon's place.
    bang_prompt: String,
    /// Model output with no newline after it yet. Kept live because it is still
    /// being written; a completed line goes straight to scrollback.
    open: String,
    /// Whether `open` is reasoning rather than the answer.
    dim: bool,
    /// Where the answer's markdown stands: what a row means depends on the
    /// rows before it, and only a fence carries that far.
    md: Markdown,
    thinking: Thinking,
    /// Finished lines waiting to be pushed above on the next render.
    /// Finished lines waiting to be pushed above on the next render.
    above: Vec<Entry>,


    /// Tool calls still running, one animated row each. A finished call
    /// replaces its row with the ✓/✗ line in scrollback, so a call that never
    /// answered would leave a spinning row behind; `abandon_tools` clears it.
    tools: Vec<RunTool>,

    /// Lines submitted while the run was working.
    queued: Vec<String>,
    /// Which row of the open list is highlighted; kept rather than the list
    /// itself, which is a function of what has been typed. `None` anchors a
    /// fresh list on its bottom row, the best match, beside the input line.
    picked: Option<usize>,
    /// The text the list was dismissed at. Any edit changes the text and the
    /// list comes back, which is what makes Esc mean "not that" rather than
    /// "never again".
    dismissed_at: Option<String>,
    /// What `/model` can complete to. A copy rather than a borrow of the
    /// config: the loop holds the session mutably while it draws.
    choices: Vec<Choice>,
    /// The same copy, of the same list `/help` prints.
    commands: Arc<Vec<Command>>,
    last_interrupt: Option<Instant>,
    /// When the last Esc was pressed, for the rewind selector's double-tap.
    last_esc: Option<Instant>,
    /// The rewind selector's rows, session order, newest last. Empty is closed;
    /// while it is open it replaces the completion list in the same rows.
    rewind: Vec<MenuEntry>,
    started: Option<Instant>,
    spinner: usize,
    /// Turns of this run that have already reported their totals.
    settled: Usage,
    /// The turn in flight, as far as the provider has said. Superseded rather
    /// than added to when its `TurnEnd` lands, or the input would count twice.
    turn: Usage,
    /// Bytes of answer and reasoning this turn has produced, which is all there
    /// is to go on until the provider reports an output count.
    produced: usize,
    /// Which halves of the settled figures came from us, not the provider.
    estimated: agent::Estimated,
    stopping: bool,
    /// Rows the view is scrolled up by. Zero shows the newest rows.
    scroll: usize,
}

/// One row either menu can offer: a completion of the line, or a message
/// from the rewind selector to go back to.
#[derive(Clone)]
enum MenuEntry {
    Completion(Candidate),
    Message { id: EntryId, show: String },
}

impl MenuEntry {
    fn show(&self) -> &str {
        match self {
            MenuEntry::Completion(c) => &c.show,
            MenuEntry::Message { show, .. } => show,
        }
    }

    fn help(&self) -> &str {
        match self {
            MenuEntry::Completion(c) => &c.help,
            MenuEntry::Message { .. } => "",
        }
    }
}

impl Ui {
    fn new(
        screen: Screen,
        keys: Arc<Keys>,
        choices: Vec<Choice>,
        commands: Arc<Vec<Command>>,
        paint: Paint,
    ) -> Self {
        let prompt = Self::paint_prompt(&paint, &paint.theme.prompt.icon);
        let bang_prompt = Self::paint_prompt(&paint, "!");
        let banner = paint.on(&paint.theme.muted, BANNER);
        let mut editor = Editor::default();
        editor.set_prompts(prompt.clone(), bang_prompt.clone());
        Self {
            screen,
            keys,
            choices,
            commands,
            editor,
            paint,
            prompt,
            bang_prompt,
            open: String::new(),
            dim: false,
            md: Markdown::default(),
            thinking: Thinking::default(),
            above: vec![Entry::Plain(banner)],

            tools: Vec::new(),
            queued: Vec::new(),
            picked: None,
            dismissed_at: None,
            last_interrupt: None,
            last_esc: None,
            rewind: Vec::new(),
            started: None,
            spinner: 0,
            settled: Usage::default(),
            turn: Usage::default(),
            produced: 0,
            estimated: agent::Estimated::default(),
            stopping: false,
            scroll: 0,
        }
    }

    /// The prompt gutter as the terminal shows it, colour and all.
    fn paint_prompt(paint: &Paint, icon: &str) -> String {
        format!("{} ", paint.on(&paint.theme.prompt.color, icon))
    }
    fn say(&mut self, line: impl Into<String>) {
        self.above.push(Entry::Plain(line.into()));
    }

    /// Where a finished row goes: a reasoning line into the streaming block's
    /// foldable entry, anything else straight into scrollback.
    fn land(&mut self, painted: String, dim: bool) {
        if dim
            && let Some(id) = self.thinking.streaming
        {
            if let Some(Entry::Folded { lines, .. }) = self.open_entry(id) {
                lines.push(painted);
                return;
            }
        // The block's first line: born the way `ctrl+t` last left
        // the last block — its own fold, not the switch.
            self.above.push(Entry::Folded {
                block: id,
                lines: vec![painted],
                folded: self.thinking.birth_fold(),
            });
            return;
        }
        self.above.push(Entry::Plain(painted));
    }

    /// The scrollback entry for a streaming block, if it has one yet.
    fn open_entry(&mut self, id: u64) -> Option<&mut Entry> {
        self.above
            .iter_mut()
            .rev()
            .find(|e| matches!(e, Entry::Folded { block, .. } if *block == id))
    }


    /// End the open paragraph and send it up into scrollback.
    /// A finished row, styled for what it is: reasoning, or the answer's
    /// markdown. The one place either decision is made.
    fn paint_row(&mut self, line: &str, dim: bool) -> String {
        if dim {
            return self.paint.on(&self.paint.theme.muted, line);
        }
        let painted = self.md.line(line, &self.paint);
        self.md.advance(line);
        painted
    }

    fn close(&mut self) {
        if !self.open.is_empty() {
            let text = std::mem::take(&mut self.open);
            let painted = self.paint_row(&text, self.dim);
            self.land(painted, self.dim);
        }
        if self.dim {
            // The block is over: it stops taking lines; its entry is already
            // in the scrollback, folded or not.
            self.thinking.close_block();
        } else {
            // A fence the answer left open stays open only within the answer.
            // A tool call ends the block, and the block is as far as markdown
            // state can honestly reach.
            self.md.reset();
        }
        self.dim = false;
    }

    fn write(&mut self, delta: &str, dim: bool) {
        self.produced += delta.len();
        if dim != self.dim {
            self.close();
            self.dim = dim;
            if dim {
                // A new reasoning block: `close` just settled the previous
                // one; this one gets a fresh id and pushes the old last one
                // back to the switch.
                self.thinking.start(&mut self.above);
            }
        }

        self.open.push_str(delta);
        // A finished line is no longer changing, so it belongs in the
        // scrollback rather than in the region we repaint: reasoning into
        // the streaming block's foldable entry, answer text as a plain row.
        while let Some(i) = self.open.find('\n') {
            let line: String = self.open.drain(..=i).collect();
            let line = line.trim_end_matches('\n').to_string();
            let painted = self.paint_row(&line, dim);
            self.land(painted, dim);
        }
    }

    fn on_event(&mut self, event: Event) {
        match &event {
            Event::TextDelta(d) => self.write(d, false),
            Event::ReasoningDelta(d) => self.write(d, true),
            Event::Usage(usage) => {
                // A retry sends a second one for the same turn: the count it
                // carries replaces the abandoned attempt's rather than joining
                // it, and the bytes that attempt produced go with it.
                self.turn = *usage;
                self.produced = 0;
            }
            Event::TurnEnd {
                usage, estimated, ..
            } => {
                self.estimated |= *estimated;
                self.settled.input += usage.input;
                self.settled.output += usage.output;
                self.settled.cache_read += usage.cache_read;
                self.settled.cache_write += usage.cache_write;
                self.turn = Usage::default();
                self.produced = 0;
            }
            Event::TurnStart { .. } => {}
            // A call's two events are one line here: the start takes a row in
            // the live region (where the spinner can animate it), and the end
            // scrolls that row up as its ✓/✗ line. Parallel calls each hold a
            // row, matched back by id because they end out of order.
            Event::ToolStart { id, name, args, .. } => {
                self.close();
                self.tools.push(RunTool {
                    id: id.clone(),
                    name: name.clone(),
                    summary: render::summarize(args),
                });
            }
            Event::ToolEnd { id, .. } => {
                self.close();
                self.tools.retain(|t| t.id != *id);
                if let Some(said) = render::describe(&event, &self.paint, self.screen.usable()) {
                    // Row by row: a scrollback line is written with a carriage
                    // return of its own, and an embedded newline would stair-
                    // step down the screen without one.
                    self.above.extend(said.lines().map(str::to_string).map(Entry::Plain));
                }
            }
            _ => {
                self.close();
                if let Some(said) = render::describe(&event, &self.paint, self.screen.usable()) {
                    // Row by row: a scrollback line is written with a carriage
                    // return of its own, and an embedded newline would stair-
                    // step down the screen without one.
                    self.above.extend(said.lines().map(str::to_string).map(Entry::Plain));
                }
            }
        }
    }

    /// A run that ended without answering a call leaves its animated row
    /// dangling. The call's own end event is never sent — a cancelled run
    /// returns before its results are reported — so give the scrollback the
    /// start line the row stood for and clear the row.
    fn abandon_tools(&mut self) {
        for t in std::mem::take(&mut self.tools) {
            self.say(
                self.paint
                    .on(&self.paint.theme.muted, &tool_start_line(&t.name, &t.summary)),
            );
        }
    }

    /// What the line could still become: a completion while a command word is
    /// being typed, or — with the rewind selector open — the user messages a
    /// conversation can be rewound to. Never during a run, when the editor is
    /// a queue, not a command line.
    fn menu(&self) -> Vec<MenuEntry> {
        if self.started.is_some() {
            return Vec::new();
        }
        if !self.rewind.is_empty() {
            return self.rewind.clone();
        }
        if self.dismissed_at.as_deref() == Some(self.editor.text()) {
            return Vec::new();
        }
        // Bottom-up: the best match belongs on the row right above the input.
        repl::complete(self.editor.text(), &self.commands, &self.choices)
            .into_iter()
            .rev()
            .map(MenuEntry::Completion)
            .collect()
    }

    /// Open the rewind selector on the given messages, newest selected first.
    fn open_rewind(&mut self, entries: Vec<MenuEntry>) {
        self.picked = Some(entries.len().saturating_sub(1));
        self.rewind = entries;
    }

    /// The highlighted row, clamped: the list shrinks as the word grows.
    fn highlighted(&self) -> Option<MenuEntry> {
        let mut menu = self.menu();
        if menu.is_empty() {
            return None;
        }
        let at = self.picked.unwrap_or(menu.len() - 1).min(menu.len() - 1);
        Some(menu.swap_remove(at))
    }

    /// The menu's rows as ratatui list items. The selected row is styled by
    /// the list itself; everything else sits muted.
    fn menu_items(&self, menu: &[MenuEntry]) -> Vec<ListItem<'static>> {
        let head = menu.iter().map(|c| c.show().len()).max().unwrap_or(0);
        let muted = self.rat_style(&self.paint.theme.muted);
        menu.iter()
            .map(|c| {
                let line = format!("  {:head$}  {}", c.show(), c.help());
                ListItem::new(Line::from(Span::styled(line, muted)))
            })
            .collect()
    }

    /// A theme style, as ratatui sees it.
    fn rat_style(&self, s: &ThemeStyle) -> RStyle {
        screen::parse_sgr(s.codes(), RStyle::default())
    }


    /// The rows above the input line: running tools, the open stream, and
    /// the status line. The editor draws separately, pinned to the bottom.
    fn live(&self, room: usize) -> Vec<String> {
        let width = self.screen.usable();
        let mut rows = Vec::new();

        for t in &self.tools {
            let line = tool_row(self.spinner, &t.name, &t.summary);
            rows.extend(screen::fit(
                &self.paint.on(&self.paint.theme.muted, &line),
                width,
            ));
        }

        rows.extend(body(
            &self.thinking,
            &self.above,
            &self.md,
            self.dim,
            &self.open,
            (width, room),
            &self.paint,
        ));

        if let Some(since) = self.started {
            let line = status::line(
                self.spinner,
                since.elapsed(),
                &counts(&self.settled, &self.turn, self.produced, self.estimated),
                self.queued.len(),
                self.stopping,
            );
            rows.extend(screen::fit(
                &self.paint.on(&self.paint.theme.muted, &line),
                width,
            ));
        }

        rows
    }

    fn set_theme(&mut self, theme: Arc<render::Theme>) {
        self.paint.theme = theme;
        self.prompt = Self::paint_prompt(&self.paint, &self.paint.theme.prompt.icon);
        self.bang_prompt = Self::paint_prompt(&self.paint, "!");
        self.editor
            .set_prompts(self.prompt.clone(), self.bang_prompt.clone());
        // above[0] is the banner, painted once at construction; restyle it so
        // a /reload lands on the new theme instead of the old.
        if let Some(Entry::Plain(s)) = self.above.first_mut() {
            *s = self.paint.on(&self.paint.theme.muted, BANNER);
        }


    }

    fn flush(&mut self) {
        let menu = self.menu();
        let width = self.screen.usable();
        let (input, caret) = self.editor.view(width);
        // A paste taller than the terminal must not push the editor area off
        // the bottom; the editor scrolls to keep the caret's row visible.
        let editor_h = input
            .len()
            .min((self.screen.height as usize).saturating_sub(1));
        let editor_top = (caret.0 as usize + 1).saturating_sub(editor_h);
        let input_view: Vec<String> = input
            .into_iter()
            .skip(editor_top)
            .take(editor_h)
            .collect();
        let caret_in_view = (caret.0 as usize).saturating_sub(editor_top);
        // From the bottom up: the input line is pinned, the menu sits above
        // it, and the scrolled history fills what is left. The caret's row
        // therefore depends only on the pinned rows, never on how the
        // history wraps.
        let menu_h = if menu.is_empty() {
            0
        } else {
            menu.len()
                .min((self.screen.height as usize).saturating_sub(editor_h + 1))
        };
        let hist_view = (self.screen.height as usize)
            .saturating_sub(editor_h + menu_h)
            .max(1);
        let live = self.live(hist_view);
        // Measured in rows, not lines: a line wider than the terminal wraps
        // into several, and counting lines here would put more rows in the
        // area than fit — pushing the newest ones off the bottom, underneath
        // the input, where nothing shows them.
        let scrollback = ScrollbackRows::new(&self.above, &self.paint);
        let (rows, scroll) = screen::window(
            scrollback.chain(live.iter().map(|s| Cow::Borrowed(s.as_str()))),
            width,
            hist_view,
            self.scroll,
        );

        self.scroll = scroll;
        let items = self.menu_items(&menu);
        let picked = self
            .picked
            .unwrap_or(menu.len().saturating_sub(1))
            .min(menu.len().saturating_sub(1));
        let highlight = self.rat_style(&self.paint.theme.menu.selected);
        let _ = self.screen.draw(|frame| {
            let area = frame.area();
            let chunks = Layout::vertical([
                Constraint::Fill(1),
                Constraint::Length(menu_h as u16),
                Constraint::Length(editor_h as u16),
            ])
            .split(area);
            let (main, menu_area, editor_area) = (chunks[0], chunks[1], chunks[2]);
            frame.render_widget(Rows(&rows), main);
            if !items.is_empty() {
                let mut state = ListState::default();
                state.select(Some(picked));
                frame.render_stateful_widget(
                    List::new(items).highlight_style(highlight),
                    menu_area,
                    &mut state,
                );
            }
            frame.render_widget(Rows(&input_view), editor_area);
            let caret_row = editor_area.y + caret_in_view as u16;
            frame.set_cursor_position((caret.1, caret_row));
        });
    }

    /// Rebuild the history from the transcript, forgetting everything the old
    /// drawing showed: a rewind changes what the conversation is, and the
    /// screen has to show the new one, not the old one with a note on it.
    fn rebuild(&mut self, session: &agent::session::Session) {
        self.above.clear();
        self.open.clear();
        self.dim = false;
        self.tools.clear();
        self.thinking.streaming = None;
        self.thinking.last = self.thinking.folded;
        self.md.reset();
        self.scroll = 0;
        self.above = render_log(
            session,
            &self.paint,
            &self.prompt,
            &self.bang_prompt,
            self.screen.usable(),
            self.thinking.folded,
        );
    }


    /// Accept a submitted input: echo it so the prompt survives the editor
    /// being cleared, then fold the block that was current back to the switch
    /// — the input pushes it out of current no matter what it turns out to be.
    fn submit(&mut self, line: &str) {
        let mut rows = Vec::new();
        push_prompt_lines(&mut rows, line, &self.prompt, &self.bang_prompt);
        self.above.extend(rows);
        self.thinking.fold_previous(&mut self.above);
    }


    fn key(&mut self, event: TermEvent, running: bool) -> Act {
        let key = match event {
            TermEvent::Resize(w, h) => {
                self.screen.resized(w, h);
                return Act::None;
            }
            TermEvent::Paste(text) => {
                self.last_esc = None;
                self.editor.insert_str(&text.replace('\r', "\n"));
                return Act::None;
            }
            TermEvent::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_view(true),
                    MouseEventKind::ScrollDown => self.scroll_view(false),
                    _ => {}
                }
                return Act::None;
            }
            // Windows reports both press and release; acting on each would
            // double every keystroke. Any key other than Esc breaks the
            // rewind double-tap: an armed press that typing interrupted must
            // not fire later.
            TermEvent::Key(k) if k.kind != KeyEventKind::Release => {
                if k.code != KeyCode::Esc {
                    self.last_esc = None;
                }
                k
            }
            _ => return Act::None,
        };
        let press = Press::of(key.code, key.modifiers);
        let bound = self.keys.action(press, !self.menu().is_empty(), running);
        // A key that is not one of the selector's own closes it first: it
        // means "drop this and keep typing", the way editing the line
        // dismisses the completion list.
        if !self.rewind.is_empty()
            && !matches!(
                bound,
                Some(
                    Action::MenuDismiss
                        | Action::MenuAccept
                        | Action::MenuNext
                        | Action::MenuPrevious
                        | Action::LineSubmit
                )
            )
        {
            self.rewind.clear();
        }

        match bound {
            Some(Action::LineClear) => return self.interrupt_or_clear(running),
            Some(Action::LineSubmit) => {
                // Enter while a menu is open runs what it highlights. The
                // typed text is a prefix; the highlighted word is the intent.
                // The menu reads the editor, so pick before draining it.
                match self.highlighted() {
                    Some(MenuEntry::Message { id, .. }) => {
                        self.rewind.clear();
                        return Act::Rewind(id);
                    }
                    Some(MenuEntry::Completion(c)) => {
                        let line = c.line;
                        // The completion's line is what runs; the typed prefix
                        // that produced it goes, so it cannot be re-submitted
                        // as a stray prompt later.
                        self.editor.take();
                        return if line.trim().is_empty() {
                            Act::None
                        } else {
                            Act::Submit(line)
                        };
                    }
                    None => {
                        let typed = self.editor.take();
                        return if typed.trim().is_empty() {
                            Act::None
                        } else {
                            Act::Submit(typed)
                        };
                    }
                }
            }
            Some(Action::RunInterrupt) => return Act::Interrupt,
            Some(Action::Rewind) => {
                // Double Esc with an empty line opens the rewind selector.
                // The first press only arms it; the second, inside the
                // window, asks the loop for the session's messages.
                if !self.editor.is_empty() {
                    return Act::None;
                }
                let now = Instant::now();
                if double_tap(&mut self.last_esc, now) {
                    self.last_esc = None;
                    return Act::OpenRewind;
                }
                return Act::None;
            }
            Some(Action::AppExit) => {
                return if self.editor.is_empty() && !running {
                    Act::Quit
                } else {
                    self.editor.delete();
                    Act::None
                };
            }
            _ => {}
        }

        match bound {
            Some(Action::InsertNewline) => self.editor.insert('\n'),
            Some(Action::DeleteCharBack) => self.editor.backspace(),
            Some(Action::DeleteCharForward) => self.editor.delete(),
            Some(Action::DeleteWordBack) => self.editor.kill_word_back(),
            Some(Action::DeleteToLineEnd) => self.editor.kill_to_end(),
            Some(Action::DeleteToLineStart) => self.editor.kill_to_start(),
            Some(Action::MoveCharLeft) => self.editor.left(),
            Some(Action::MoveCharRight) => self.editor.right(),
            Some(Action::MoveWordLeft) => self.editor.word_left(),
            Some(Action::MoveWordRight) => self.editor.word_right(),
            Some(Action::MoveLineStart) => self.editor.home(),
            Some(Action::MoveLineEnd) => self.editor.end(),
            Some(Action::HistoryOlder) => self.editor.up(),
            Some(Action::HistoryNewer) => self.editor.down(),
            Some(Action::AppClearScreen) => self.screen.clear(),
            Some(Action::ScrollUp) => self.scroll_view(true),
            Some(Action::ScrollDown) => self.scroll_view(false),
            Some(Action::ThinkFold) => {
                // The last block only: the one streaming, or the newest
                // finished one when nothing is. The switch is left alone, so
                // the blocks no one is touching keep what they had.
                self.thinking.toggle_current(&mut self.above);
            }
            Some(Action::ThinkFoldAll) => {
                // Every block in the scrollback, the last one included, and
                // the switch with them: one key presses the whole screen to a
                // single state.
                self.thinking.flip_all(&mut self.above);
            }

            Some(Action::MenuAccept) => {
                match self.highlighted() {
                    Some(MenuEntry::Message { id, .. }) => {
                        self.rewind.clear();
                        return Act::Rewind(id);
                    }
                    Some(MenuEntry::Completion(c)) => {
                        self.editor.set_line(&c.line);
                        // Something still expected after it wants a space first.
                        if c.more {
                            self.editor.insert(' ');
                        }
                        self.picked = None;
                    }
                    None => {}
                }
            }
            Some(Action::MenuNext) => {
                let n = self.menu().len().saturating_sub(1);
                let at = self.picked.unwrap_or(n).min(n);
                self.picked = Some(at.saturating_add(1).min(n));
            }
            Some(Action::MenuPrevious) => {
                let n = self.menu().len().saturating_sub(1);
                let at = self.picked.unwrap_or(n).min(n);
                self.picked = Some(at.saturating_sub(1));
            }
            Some(Action::MenuDismiss) => {
                let was_rewind = !self.rewind.is_empty();
                self.rewind.clear();
                // The completion list is recorded against the text, so any
                // edit brings it back: this means "not that", not "never
                // again". The rewind selector dismisses without recording,
                // so the completion list stays available after it.
                if !was_rewind {
                    self.dismissed_at = Some(self.editor.text().to_string());
                }
            }
            // Unbound and printable is the one thing no table has to say.
            None => {
                if let KeyCode::Char(c) = key.code
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    self.editor.insert(c);
                }
            }
            Some(
                Action::LineClear
                | Action::LineSubmit
                | Action::RunInterrupt
                | Action::AppExit
                | Action::Rewind,
            ) => unreachable!("handled above"),
        }
        Act::None
    }

    /// Nudge the scrolled history window one half screen, the same step
    /// PageUp/PageDown and the mouse wheel take.
    fn scroll_view(&mut self, up: bool) {
        let step = (self.screen.height as usize) / 2;
        self.scroll = if up {
            self.scroll.saturating_add(step)
        } else {
            self.scroll.saturating_sub(step)
        };
    }

    /// One key, three meanings, and the escalation travels with the binding
    /// rather than with Ctrl-C: stop the run, clear the line, or — pressed
    /// twice inside the window — leave.
    fn interrupt_or_clear(&mut self, running: bool) -> Act {
        if double_tap(&mut self.last_interrupt, Instant::now()) {
            return Act::Quit;
        }
        if running {
            return Act::Interrupt;
        }
        if self.editor.is_empty() {
            self.say(
                self.paint
                    .on(&self.paint.theme.muted, "press it again to quit"),
            );
        } else {
            self.editor.clear();
        }
        Act::None
    }


}

/// What the status line should say the run has cost.
///
/// The turn in flight contributes only what the provider has already stated —
/// the input count on the Anthropic wire, nothing at all on the OpenAI one —
/// so its output is stood in for by the bytes that have arrived. Its measured
/// figures are replaced, never added to, when its `TurnEnd` folds them into
/// `settled`, or a turn's input would be counted twice.
fn counts(
    settled: &Usage,
    turn: &Usage,
    produced: usize,
    estimated: agent::Estimated,
) -> status::Counts {
    let output_exact = turn.output > 0 && !estimated.output;
    status::Counts {
        input: settled.input + turn.input,
        output: settled.output
            + if output_exact {
                turn.output
            } else {
                brain::estimate::bytes(produced) as u64
            },
        // The turn in flight contributes an input count only once the provider
        // has stated one, so the settled half is what decides this.
        input_exact: !estimated.input,
        output_exact,
    }
}

pub struct Tui {
    core: Repl,
    ui: Ui,
    keys: UnboundedReceiver<TermEvent>,
    totals: Totals,
}

/// crossterm reads blockingly, so the keyboard gets a thread of its own and
/// reaches the loop as just another channel.
fn reader() -> UnboundedReceiver<TermEvent> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = crossterm::event::read() {
            if tx.send(event).is_err() {
                return;
            }
        }
    });
    rx
}

/// Where recalled prompts are kept between sessions.
fn history_path() -> Option<std::path::PathBuf> {
    tools::state::dir().map(|d| d.join("history"))
}

/// Enough to recall from without the file growing without bound.
const HISTORY_KEEP: usize = 1_000;

impl Tui {
    pub fn new(core: Repl, keys: Arc<Keys>) -> Result<Self> {
        let paint = Paint::with_theme(true, Arc::new(core.config.theme.clone()));
        let mut ui = Ui::new(
            Screen::new()?,
            keys,
            core.choices(),
            core.commands.clone(),
            paint,
        );
        if let Some(prior) = history_path().and_then(|p| std::fs::read_to_string(p).ok()) {
            ui.editor.seed_history(editor::decode(&prior));
        }
        // A resumed session shows its transcript from the start: the whole
        // screen is rebuildable now, so there is no reason to hide it.
        if !core.session.is_empty() {
            ui.rebuild(&core.session);
        }
        Ok(Self {
            core,
            ui,
            keys: reader(),
            totals: Totals::default(),
        })
    }

    /// Best effort: losing a recall list is not worth a message on the way out.
    fn save_history(&self) {
        let Some(path) = history_path() else { return };
        let all = self.ui.editor.history();
        let keep = &all[all.len().saturating_sub(HISTORY_KEEP)..];
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, editor::encode(keep));
    }

    pub async fn run(
        mut self,
        tx: UnboundedSender<Event>,
        mut rx: UnboundedReceiver<Event>,
    ) -> Result<()> {
        loop {
            self.ui.flush();
            let Some(key) = self.keys.recv().await else {
                break;
            };
            let line = match self.ui.key(key, false) {
                Act::Submit(line) => line,
                Act::Quit => break,
                Act::OpenRewind => {
                    self.open_rewind();
                    continue;
                }
                Act::Rewind(id) => {
                    self.rewind_turn(id);
                    continue;
                }
                Act::Interrupt | Act::None => continue,
            };
            self.ui.submit(&line);
            // A fresh turn starts at the newest row: a view scrolled up to
            // read would otherwise stream the run's output out of sight.
            self.ui.scroll = 0;
            // Written per line rather than on the way out: quitting with two
            // Ctrl-Cs skips every tidy exit path there is.
            self.save_history();
            match self.core.command(&line, &self.totals) {
                Step::Quit => break,
                Step::Bash(command) => {
                    // The command runs off the key loop so Esc can stop it,
                    // exactly as an agent turn can be interrupted.
                    let cancel = CancellationToken::new();
                    let lines = {
                        let Self { core, ui, keys, .. } = &mut self;
                        let run = core.bash(&command, cancel.clone());
                        tokio::pin!(run);
                        loop {
                            ui.flush();
                            tokio::select! {
                                done = &mut run => break done,
                                Some(key) = keys.recv() => match ui.key(key, true) {
                                    Act::Interrupt => { cancel.cancel(); ui.stopping = true; }
                                    Act::Submit(line) => ui.queued.push(line),
                                    // Nothing else can stop a command that will not stop.
                                    Act::Quit => {
                                        ui.screen.leave();
                                        std::process::exit(130)
                                    }
                                    // Esc means interrupt while the run is in flight.
                                    Act::OpenRewind | Act::Rewind(_) => {}
                                    Act::None => {}
                                },
                            }
                        }
                    };
                    self.ui.above.extend(lines.into_iter().map(Entry::Plain));
                }
                Step::Handled(lines) => {
                    self.ui.above.extend(lines.into_iter().map(Entry::Plain));
                    // The key map lives in two places; a reload has to reach
                    // both or the screen keeps answering to the old bindings.
                    if !Arc::ptr_eq(&self.ui.keys, &self.core.keys) {
                        self.ui.keys = self.core.keys.clone();
                    }
                    // Likewise the completion list: /reload is allowed to
                    // define models — and skills — the last one did not.
                    self.ui.choices = self.core.choices();
                    if self.ui.paint.theme.as_ref() != &self.core.config.theme {
                        self.ui.set_theme(Arc::new(self.core.config.theme.clone()));
                    }
                    if !Arc::ptr_eq(&self.ui.commands, &self.core.commands) {
                        self.ui.commands = self.core.commands.clone();
                    }
                }
                Step::Compact(focus) => {
                    // Long enough to want the spinner, so it borrows the run's.
                    self.ui.started = Some(Instant::now());
                    let done = self
                        .core
                        .agent
                        .compact_now(&mut self.core.session, focus.as_deref())
                        .await;
                    self.ui.started = None;
                    match done {
                        Some((report, spent)) => {
                            self.totals.merge(&spent);
                            self.ui.on_event(Event::Compacted(report));
                            if let Err(e) = self.core.save() {
                                self.ui
                                    .say(format!("warning: the transcript was not saved: {e}"));
                            }
                        }
                        None => {
                            let held = self.core.agent.kept_tokens().unwrap_or(0);
                            let now = brain::estimate::tokens(&self.core.session.context());
                            let why = format!(
                                "nothing to compact — {now} tokens, all inside the {held} \
                                 kept as working context"
                            );
                            self.ui
                                .say(self.ui.paint.on(&self.ui.paint.theme.muted, &why));
                        }
                    }
                }
                Step::Prompt(prompt) => {
                    let mut next = Some(prompt);
                    // Anything submitted while the run worked becomes the next
                    // prompt rather than waiting for the user to send it again.
                    while let Some(prompt) = next.take() {
                        self.turn(prompt, &tx, &mut rx).await;
                        if !self.ui.queued.is_empty() {
                            let queued = std::mem::take(&mut self.ui.queued).join("\n");
                            self.ui.submit(&queued);
                            next = Some(queued);
                        }
                    }
                }
            }
        }
        self.save_history();
        Ok(())
    }

    /// A message chosen from the rewind selector: cut the transcript there
    /// and say what is left.
    fn rewind_turn(&mut self, id: EntryId) {
        match self.core.rewind_to(id) {
            Ok(0) => {
                self.ui.say(
                    self.ui
                        .paint
                        .on(&self.ui.paint.theme.muted, "nothing to rewind to"),
                );
            }
            Ok(_) => {
                // The transcript is the source of truth again: rebuild the
                // whole view from it, so the screen returns to the node the
                // conversation did instead of keeping the forgotten turns.
                self.ui.rebuild(&self.core.session);
                let tail = self.core.session.live().last().map(|(_, m)| m.text());
                let at = tail
                    .filter(|t| !t.is_empty())
                    .map(|t| format!(" — the transcript now ends at {}", render::clip(&t, 60)))
                    .unwrap_or_default();
                self.ui.say(
                    self.ui
                        .paint
                        .on(&self.ui.paint.theme.muted, &format!("rewound{at}")),
                );
            }
            Err(e) => {
                self.ui
                    .say(format!("warning: the transcript was not saved: {e}"));
            }
        }
    }

    /// Open the rewind selector on every user message the conversation can
    /// go back to.
    fn open_rewind(&mut self) {
        let entries: Vec<MenuEntry> = self
            .core
            .session
            .prompts()
            .into_iter()
            .map(|(id, text)| MenuEntry::Message {
                id,
                show: render::clip(&text, 60),
            })
            .collect();
        if entries.is_empty() {
            self.ui.say(
                self.ui
                    .paint
                    .on(&self.ui.paint.theme.muted, "nothing to rewind to"),
            );
            return;
        }
        self.ui.open_rewind(entries);
    }


    async fn turn(
        &mut self,
        prompt: String,
        tx: &UnboundedSender<Event>,
        rx: &mut UnboundedReceiver<Event>,
    ) {
        self.core.session.resume(prompt);
        let cancel = CancellationToken::new();
        let ctx = self
            .core
            .ctx
            .clone()
            .with_cancel(cancel.clone())
            .with_fresh_result();

        self.ui.started = Some(Instant::now());
        self.ui.stopping = false;
        self.ui.settled = Usage::default();
        self.ui.turn = Usage::default();
        self.ui.produced = 0;
        self.ui.estimated = agent::Estimated::default();

        let out = {
            // Disjoint borrows: the run holds the session while the loop keeps
            // drawing and reading keys.
            let Self { core, ui, keys, .. } = self;
            let Repl { agent, session, .. } = core;
            let run = agent.run(session, &ctx, tx);
            tokio::pin!(run);
            let mut tick = tokio::time::interval(status::SPIN);
            loop {
                ui.flush();
                tokio::select! {
                    done = &mut run => break done,
                    Some(event) = rx.recv() => ui.on_event(event),
                    Some(key) = keys.recv() => match ui.key(key, true) {
                        Act::Interrupt => { cancel.cancel(); ui.stopping = true; }
                        Act::Submit(line) => ui.queued.push(line),
                        // Nothing else can stop a run that will not stop.
                        Act::Quit => {
                            ui.screen.leave();
                            std::process::exit(130)
                        }
                        // Esc means interrupt while the run is in flight.
                        Act::OpenRewind | Act::Rewind(_) => {}
                        Act::None => {}
                    },
                    _ = tick.tick() => ui.spinner += 1,
                }
            }
        };

        // Whatever the run posted on its way out still has to be shown.
        while let Ok(event) = rx.try_recv() {
            self.ui.on_event(event);
        }
        self.ui.close();
        // A cancelled run's calls got no `ToolEnd`; their animated rows have
        // to reach scrollback some other way before the next flush draws them
        // as a frozen spinner.
        self.ui.abandon_tools();
        self.ui.started = None;

        // Saved either way: an interrupted turn is exactly the one worth keeping.
        if let Err(e) = self.core.save() {
            self.ui
                .say(format!("warning: the transcript was not saved: {e}"));
        }

        match out {
            Ok(o) => self.totals.merge(&o.totals),
            Err(AgentError::Cancelled) => self
                .ui
                .say(self.ui.paint.on(&self.ui.paint.theme.muted, "stopped")),
            Err(e) => {
                let text = format!(
                    "{} {e}",
                    self.ui.paint.on(&self.ui.paint.theme.status.err, "error")
                );
                self.ui.say(text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Cow, Entry, ScrollbackRows, Thinking, body, counts, tool_row};
    use crate::render::Markdown;
    use crate::render::Paint;
    use brain::stream::Usage;

    /// A closed reasoning block of id `id` and `n` lines in the scrollback.
    fn block(id: u64, n: usize, folded: bool) -> Entry {
        Entry::Folded {
            block: id,
            lines: (1..=n).map(|i| format!("line {i}")).collect(),
            folded,
        }
    }

    /// What the screen shows for a run in the middle of reasoning.
    fn shown(t: &Thinking, open: &str) -> Vec<String> {
        body(
            t,
            &[],
            &Markdown::default(),
            true,
            open,
            (80, 9),
            &Paint::new(false),
        )
    }

    #[test]
    fn a_pending_tool_row_spins_and_names_its_argument() {
        assert_eq!(tool_row(0, "read", "a.rs"), "⠋ read a.rs");
        // A tool with nothing worth showing keeps the row to a name.
        assert_eq!(tool_row(5, "spinner", ""), "⠴ spinner");
    }

    #[test]
    fn a_folded_entry_is_its_summary_until_unfolded() {
        let entries = [block(1, 2, true)];
        let paint = Paint::new(false);
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(&entries, &paint).collect();
        assert_eq!(rows, vec!["thinking · 2 lines"]);
    }

    #[test]
    fn an_unfolded_entry_shows_its_lines() {
        let entries = [block(1, 2, false)];
        let paint = Paint::new(false);
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(&entries, &paint).collect();
        assert_eq!(rows, vec!["line 1", "line 2"]);
    }

    #[test]
    fn a_plain_entry_is_itself() {
        let entries = [Entry::Plain("hello".to_string())];
        let paint = Paint::new(false);
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(&entries, &paint).collect();
        assert_eq!(rows, vec!["hello"]);
    }

    #[test]
    fn an_empty_scrollback_iterates_to_nothing() {
        // The front walk used to index `entries[0]` before any check, so a
        // forward iteration over an empty scrollback panicked.
        let paint = Paint::new(false);
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(&[], &paint).collect();
        assert!(rows.is_empty());
    }

    #[test]
    fn scrollback_rows_walk_from_both_ends() {
        let entries = vec![
            Entry::Plain("a".to_string()),
            block(1, 2, false),
            Entry::Plain("d".to_string()),
        ];
        let paint = Paint::new(false);
        let rows = ScrollbackRows::new(&entries, &paint);
        let (front, back): (Vec<_>, Vec<_>) = {
            let mut f = Vec::new();
            let mut b = Vec::new();
            let mut it = rows;
            loop {
                match (it.next(), it.next_back()) {
                    (Some(x), Some(y)) => {
                        f.push(x);
                        b.push(y);
                    }
                    (Some(x), None) => f.push(x),
                    (None, Some(y)) => b.push(y),
                    (None, None) => break,
                }
            }
            (f, b)
        };
        assert_eq!(front, vec!["a", "line 1"]);
        assert_eq!(back, vec!["d", "line 2"]);
    }


    #[test]
    fn a_shut_window_is_one_row_whatever_it_holds() {
        // Including the line still arriving: it is reasoning too, and putting
        // it on screen is the window this row exists to replace.
        let t = Thinking::default();
        assert_eq!(shown(&t, "half a sentence"), vec!["thinking..."]);
    }

    #[test]
    fn an_unfolded_block_streams_its_line_live() {
        // One switch on the last block: folded, the live row is the
        // placeholder; unfolded, it is the reasoning itself.
        let mut t = Thinking::default();
        t.start(&mut []);
        t.last = false;
        assert_eq!(shown(&t, "half a sentence"), vec!["half a sentence"]);
        t.last = true;
        assert_eq!(shown(&t, "half a sentence"), vec!["thinking..."]);
    }



    #[test]
    fn a_counted_block_needs_no_live_placeholder() {
        // The count row in the scrollback already stands for the folded
        // block; a second "thinking..." live row would show it twice.
        let mut t = Thinking::default();
        t.start(&mut []);
        let above = [block(1, 1, true)];
        let rows = body(
            &t,
            &above,
            &Markdown::default(),
            true,
            "half a sentence",
            (80, 9),
            &Paint::new(false),
        );
        assert!(rows.is_empty());
    }


    #[test]
    fn toggling_moves_the_last_block_and_nothing_else() {
        // `ctrl+t` flips the block that is last now, and only it: the block
        // pushed out of last by the new one folds back to the switch.
        let mut t = Thinking::default();
        let mut above = vec![Entry::Folded {
            block: 9,
            lines: vec!["old".to_string()],
            folded: false,
        }];
        t.start(&mut above);
        above.push(Entry::Folded {
            block: 1,
            lines: vec!["new".to_string()],
            folded: true,
        });
        t.toggle_current(&mut above);
        assert!(t.folded);
        assert!(matches!(&above[0], Entry::Folded { folded: true, .. }));
        assert!(matches!(&above[1], Entry::Folded { folded: false, .. }));
    }

    #[test]
    fn a_flip_survives_a_block_that_never_lands_a_line() {
        // A `ctrl+t` names the last block even when it has no entry yet; if
        // the block then ends without a line, the flip stays for the next
        // block's birth — the key set the last value, not this block's.
        let mut t = Thinking::default();
        t.start(&mut []);
        let mut above: Vec<Entry> = Vec::new();
        t.toggle_current(&mut above);
        t.close_block();
        assert!(!t.birth_fold());
    }

    #[test]
    fn a_finished_block_keeps_its_fold_until_the_next_question() {
        // An unfold survives the answer — a finished block is still last —
        // and folds back to the switch the moment a new input is submitted.
        let mut t = Thinking::default();
        t.start(&mut []);
        let mut above = vec![block(1, 1, t.birth_fold())];
        t.toggle_current(&mut above);
        assert!(matches!(&above[0], Entry::Folded { folded: false, .. }));
        t.close_block();
        // Still last until the next question is asked.
        assert!(matches!(&above[0], Entry::Folded { folded: false, .. }));
        t.fold_previous(&mut above);
        // The submitted question pushes it out of last: it folds to the
        // switch.
        assert!(matches!(&above[0], Entry::Folded { folded: true, .. }));
        assert!(!t.birth_fold());
    }
    #[test]
    fn a_finished_block_follows_a_global_unfold() {
        // The fold follows the switch both ways: a screen the global key
        // opened keeps its block open once the next question takes over.
        let mut t = Thinking { folded: false, ..Default::default() };
        t.start(&mut []);
        let mut above = vec![block(1, 1, t.birth_fold())];
        t.close_block();
        t.fold_previous(&mut above);
        assert!(matches!(&above[0], Entry::Folded { folded: false, .. }));
    }

    #[test]
    fn a_new_block_in_the_same_answer_folds_the_previous_and_inherits_the_flip() {
        // A second reasoning block in the same answer is the new last: the
        // first one folds back to the switch, and the second is born the way
        // `ctrl+t` left the last block.
        let mut t = Thinking::default();
        t.start(&mut []);
        let mut above = vec![block(1, 1, t.birth_fold())];
        t.toggle_current(&mut above);
        assert!(matches!(&above[0], Entry::Folded { folded: false, .. }));
        t.close_block();
        t.start(&mut above);
        above.push(block(2, 1, t.birth_fold()));
        assert!(matches!(&above[0], Entry::Folded { folded: true, .. }));
        assert!(matches!(&above[1], Entry::Folded { folded: false, .. }));
    }

    #[test]
    fn a_flip_before_the_first_line_lands_on_birth() {
        // A `ctrl+t` on a block with no entry yet cannot flip anything on
        // screen; it flips the last value, so the block is born the way the
        // key asked — and the switch is not touched.
        let mut t = Thinking::default();
        t.start(&mut []);
        let mut above: Vec<Entry> = Vec::new();
        t.toggle_current(&mut above);
        assert!(t.folded);
        assert!(!t.birth_fold());
        // The flip is the last value now, not a one-shot: the next birth
        // takes it too.
        assert!(!t.birth_fold());
    }


    #[test]
    fn the_live_placeholder_follows_the_streaming_entry() {
        // Once the block has an entry, the live region reads its own state,
        // not the last value: a block the user unfolded streams its lines
        // even though the switch still says folded.
        let mut t = Thinking::default();
        t.start(&mut []);
        assert!(t.holds(true, &[]));
        let above = [block(1, 1, false)];
        assert!(!t.holds(true, &above));
    }


    #[test]
    fn a_global_flip_takes_the_current_block_with_it() {
        // The case that named the key: everything else unfolded, the current
        // block folded on its own. The global key folds the whole screen —
        // the current block keeps its fold, because the fold is where the
        // rest are going.
        let mut t = Thinking { folded: false, ..Default::default() };
        t.start(&mut []);
        let mut above = vec![Entry::Folded {
            block: 1,
            lines: vec!["new".to_string()],
            folded: true,
        }];
        t.flip_all(&mut above);
        assert!(t.folded);
        assert!(matches!(&above[0], Entry::Folded { folded: true, .. }));
    }


    #[test]
    fn flipping_every_block_moves_the_switch_with_them() {
        // The global key folds or unfolds every block, the current one
        // included, and moves the switch with them: entries and switch never
        // disagree, so the screen always folds back to a single state.
        let mut t = Thinking::default();
        t.start(&mut []);
        let mut above = vec![block(1, 1, true)];
        t.toggle_current(&mut above); // unfold the current block on its own
        t.close_block();
        t.flip_all(&mut above); // global fold
        assert!(!t.folded);
        assert!(above
            .iter()
            .all(|e| matches!(e, Entry::Folded { folded: false, .. })));
        // The switch moved with them, so the next block is born unfolded.
        assert!(!t.birth_fold());
        // And a second global press folds the whole screen back.
        t.flip_all(&mut above);
        assert!(t.folded);
        assert!(above
            .iter()
            .all(|e| matches!(e, Entry::Folded { folded: true, .. })));
    }


    #[test]
    fn refolding_hides_lines_the_reader_has_already_seen() {
        // The screen repaints from scrollback every frame, so lines already
        // shown are still the same entry: folding them takes them back.
        let mut entry = block(1, 2, false);
        let paint = Paint::new(false);
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(std::slice::from_ref(&entry), &paint).collect();
        assert_eq!(rows, vec!["line 1", "line 2"]);
        if let Entry::Folded { folded, .. } = &mut entry {
            *folded = true;
        }
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(std::slice::from_ref(&entry), &paint).collect();
        assert_eq!(rows, vec!["thinking · 2 lines"]);
    }


    #[test]
    fn the_answer_is_never_folded() {
        let t = Thinking::default();
        assert!(!t.holds(false, &[]));
        let rows = body(
            &t,
            &[],
            &Markdown::default(),
            false,
            "hello",
            (80, 9),
            &Paint::new(false),
        );
        assert_eq!(rows, vec!["hello"]);
    }

    #[test]
    fn a_long_paragraph_is_trimmed_to_the_room_it_is_given() {
        let t = Thinking::default();
        let rows = body(
            &t,
            &[],
            &Markdown::default(),
            false,
            &"x".repeat(500),
            (80, 3),
            &Paint::new(false),
        );
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn a_turn_that_has_only_started_shows_its_input_and_guesses_its_output() {
        let turn = Usage {
            input: 8_400,
            ..Default::default()
        };
        let c = counts(&Usage::default(), &turn, 1_536, Default::default());
        assert_eq!((c.input, c.output, c.output_exact), (8_400, 512, false));
    }

    #[test]
    fn a_later_turn_adds_to_what_the_earlier_ones_measured() {
        // The guess covers only the turn in flight; the settled figures behind
        // it are measured and must not be re-guessed.
        let settled = Usage {
            input: 10_000,
            output: 600,
            ..Default::default()
        };
        let turn = Usage {
            input: 2_000,
            ..Default::default()
        };
        let c = counts(&settled, &turn, 300, Default::default());
        assert_eq!((c.input, c.output, c.output_exact), (12_000, 700, false));
    }

    #[test]
    fn a_measured_output_supersedes_the_guess_for_the_same_turn() {
        let turn = Usage {
            input: 2_000,
            output: 90,
            ..Default::default()
        };
        let c = counts(&Usage::default(), &turn, 9_000, Default::default());
        assert_eq!((c.output, c.output_exact), (90, true));
    }

    #[test]
    fn a_turn_the_provider_did_not_measure_stays_marked() {
        // The figures are real numbers and still not the provider's; a status
        // line that dropped the tilde would be claiming they were.
        let turn = Usage {
            input: 8_400,
            output: 300,
            ..Default::default()
        };
        let guessed = agent::Estimated {
            input: true,
            output: true,
            ..Default::default()
        };
        assert!(!counts(&Usage::default(), &turn, 0, guessed).output_exact);
        assert!(!counts(&Usage::default(), &turn, 0, guessed).input_exact);
        let measured = counts(&Usage::default(), &turn, 0, Default::default());
        assert!(measured.output_exact && measured.input_exact);
    }
    #[test]
    fn a_flip_applies_to_each_new_last_block_until_flipped_back() {
        // `ctrl+t` controls the last thinking block, whatever it is: the
        // first one is born unfolded, and each new block that takes over as
        // last is born unfolded too, while the one it displaces folds back to
        // the switch.
        let mut t = Thinking::default();

        // Startup: the key names a block that does not exist yet.
        t.toggle_current(&mut []);
        assert!(!t.birth_fold());

        // The first thinking block arrives and is the last one.
        t.start(&mut []);
        let mut above = vec![block(1, 1, t.birth_fold())];
        assert!(matches!(&above[0], Entry::Folded { folded: false, .. }));
        t.close_block();

        // A tool call ends the block; the next thinking block is the new
        // last, born unfolded, and the first one folds back to the switch.
        t.start(&mut above);
        above.push(block(2, 1, t.birth_fold()));
        assert!(matches!(&above[0], Entry::Folded { folded: true, .. }));
        assert!(matches!(&above[1], Entry::Folded { folded: false, .. }));
    }
}
