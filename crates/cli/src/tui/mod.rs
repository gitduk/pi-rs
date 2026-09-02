//! The interactive surface: one owner of the terminal for the whole session.
//!
//! The line-editing library that used to sit here owned the terminal only while
//! it was reading a line, which is what made a key press during a run
//! unreachable and left the renderer writing into a terminal nobody was
//! managing. Here a single loop holds raw mode from start to finish and
//! services three sources at once — the agent's events, the keyboard, and a
//! timer for the spinner — so nothing has to be bolted on beside it.

mod editor;
mod row;
mod screen;
mod settings;

use std::borrow::Cow;
use std::collections::HashSet;
use std::time::Instant;

use agent::session::{Entry as LogEntry, EntryId, Node, UserBody};
use agent::{AgentError, Event, Totals};
use anyhow::Result;
use brain::message::{AssistantContent, ReasoningContent};
use brain::stream::Usage;
use crossterm::event::{Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::journal;
use crate::keys::{Action, Keys, Press};
use crate::render::Style as ThemeStyle;
use crate::render::{self, Markdown, Paint};
use crate::repl::{self, Candidate, Choice, Command, Fate, Repl, Rewound, Step};
use crate::session::ResumeChoice;
use editor::Editor;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style as RStyle;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};
use row::Row;
use screen::{Rows, Screen};
use std::sync::Arc;

// What a folded run shows instead of what it is thinking.
const THINKING: &str = "thinking...";

// How close two Ctrl-C presses must be to read as one deliberate quit.
//
// Borrowed from pi, which uses the same 500ms. A latching flag looks simpler
// and is wrong: clear one half-typed line, type another, clear that — and the
// second clear reads as the second half of a double-tap and quits.
const DOUBLE_TAP: std::time::Duration = std::time::Duration::from_millis(500);

// Whether a press lands inside the double-tap window of the previous one,
// and records the press either way.
fn double_tap(last: &mut Option<Instant>, now: Instant) -> bool {
    let hit = last.is_some_and(|p| now.duration_since(p) < DOUBLE_TAP);
    *last = Some(now);
    hit
}

// A `/settings set <path> <value>` whose path names a secret: the value must
// not enter the recall history, which is written to disk in the clear.
fn secret_settings_set(line: &str) -> bool {
    match repl::parse(line) {
        Some(repl::Cmd::Settings(rest)) => {
            let mut parts = rest.splitn(3, char::is_whitespace);
            matches!(parts.next(), Some("set"))
                && parts.next().is_some_and(|p| journal::secret(journal::leaf(p)))
        }
        _ => false,
    }
}
// What a key press asked the loop to do. Every press redraws regardless.
#[derive(Debug, PartialEq, Eq)]
enum Act {
    None,
    Submit(String),
    Interrupt,
    /// The rewind selector wants everywhere the session can go back to.
    OpenRewind,
    /// Esc caught a prompt on its way out: stop the run, then unsend it.
    Unsend,
    /// A row chosen from the rewind selector: the conversation rewinds there,
    /// and what the row was decides whether it is kept or unsent.
    Rewind(EntryId),
    /// The settings panel submitted an edited value.
    CommitSetting(String, String),
    /// `ctrl+l` twice: a fresh session, the old one kept on disk, and the
    /// screen is rebuilt from the empty one.
    NewSession,
    Quit,
}
// What the workspace-dependent completions answer with, each read the first
// time one is asked for.
//
// Lazy because reading the sessions means opening every archive for this
// workspace and listing the worktrees forks git, while most runs type neither
// `/resume` nor `/worktree` — reading them up front was the whole of a
// noticeable startup pause. Neither command's bare form comes through here;
// both ask directly, as they always did.
struct Lists {
    store: crate::session::Store,
    workspace: std::path::PathBuf,
    sessions: std::cell::OnceCell<Vec<ResumeChoice>>,
    worktrees: std::cell::OnceCell<Vec<Choice>>,
}

impl Lists {
    fn new(store: crate::session::Store, workspace: std::path::PathBuf) -> Self {
        Self {
            store,
            workspace,
            sessions: std::cell::OnceCell::new(),
            worktrees: std::cell::OnceCell::new(),
        }
    }

    fn sessions(&self) -> &[ResumeChoice] {
        self.sessions
            .get_or_init(|| self.store.choices(&self.workspace))
    }

    /// The checkouts named with the branch each is on. Empty outside a git
    /// repository, which is where it belongs: `/worktree` has nothing to offer
    /// there.
    fn worktrees(&self) -> &[Choice] {
        self.worktrees.get_or_init(|| {
            crate::worktree::list(&self.workspace)
                .map(|trees| {
                    trees
                        .into_iter()
                        .map(|t| Choice {
                            note: t.branch.unwrap_or_else(|| "detached HEAD".into()),
                            name: t.name,
                        })
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    /// A turn or a switch can change what either list would say — a session
    /// saved, a worktree the model added. Dropped rather than recomputed:
    /// whoever asks next pays, and most of the time nobody does.
    fn forget(&mut self) {
        self.sessions.take();
        self.worktrees.take();
    }

    /// Point at a workspace, dropping what the last one answered with. Both
    /// lists are keyed by it, so after a `/worktree` move neither is merely
    /// stale — each is another tree's.
    fn at(&mut self, workspace: &std::path::Path) {
        self.workspace = workspace.to_path_buf();
        self.forget();
    }
}

// Whether reasoning is folded to its count line, and which block the stream
// is filling right now.
//
// Thinking always lives in a foldable scrollback entry, folded or not: the
// screen is repainted from its rows every frame, so a line already shown
// can still be folded. A block's own state lasts only while it is last; the
// next block pushes it back to `folded`, the switch.
struct Thinking {
    /// The next block id; closed rows keep the id they were born with, so
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

// Shut: the reasoning is worth a glance while it runs and almost never worth
// the scrollback it costs afterwards.
//
// The only constructor, because a derived one would answer `false` here — the
// opposite of what the type says two lines up, in the one place nobody would
// think to look.
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
    fn holds(&self, reasoning: bool, scrollback: &[Row]) -> bool {
        reasoning && self.stream_fold(scrollback)
    }

    /// How the block streaming now is folded: its entry's own state, or —
    /// before its first line lands — the last value.
    fn stream_fold(&self, scrollback: &[Row]) -> bool {
        if let Some(id) = self.streaming
            && let Some(folded) = scrollback
                .iter()
                .rev()
                .find(|r| r.block() == Some(id))
                .and_then(Row::folded)
        {
            return folded;
        }
        self.last
    }

    /// The block that was last stops being so: it folds back to the switch.
    /// A new input and a new block both push it out of last.
    fn retire_last(&mut self, scrollback: &mut [Row]) {
        if let Some(row) = last_folded(scrollback) {
            row.set_folded(self.folded);
        }
    }

    /// The next block id. The one place ids come from, so a rebuilt block and
    /// a streamed one can never mean the same number.
    fn take_id(&mut self) -> u64 {
        let id = self.next;
        self.next += 1;
        id
    }

    /// A new reasoning block is about to start. It gets an id the scrollback
    /// entry will be born with; its first line takes `birth_fold`.
    fn start(&mut self, scrollback: &mut [Row]) {
        self.retire_last(scrollback);
        self.streaming = Some(self.take_id());
    }

    /// The block that was last stops being so the moment a new input is
    /// submitted: it folds to the switch, its unfold lasting only while it
    /// was last.
    fn fold_previous(&mut self, scrollback: &mut [Row]) {
        self.retire_last(scrollback);
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
    /// them, the last block included: rows and switch must never disagree,
    /// or the next block is born with a stale value and a mixed screen can
    /// never fold back to a single state.
    fn flip_all(&mut self, scrollback: &mut [Row]) {
        self.folded = !self.folded;
        self.last = self.folded;
        for row in scrollback.iter_mut() {
            row.set_folded(self.folded);
        }
    }

    /// Flip the last block only: the one streaming, or the newest finished
    /// one when nothing is. The switch is left alone, so the blocks no one is
    /// touching keep what they had; a block with no entry yet is born with
    /// the flip.
    fn toggle_current(&mut self, scrollback: &mut [Row]) {
        self.last = !self.last;
        let flipped = if let Some(id) = self.streaming {
            scrollback.iter_mut().rev().find(|r| r.block() == Some(id))
        } else {
            last_folded(scrollback)
        };
        if let Some(row) = flipped {
            row.set_folded(self.last);
        }
    }
}

// The newest reasoning block's entry in the scrollback, if any.
fn last_folded(scrollback: &mut [Row]) -> Option<&mut Row> {
    scrollback.iter_mut().rev().find(|r| r.block().is_some())
}

// The scrollback as rows, walked from either end without flattening the
// whole history: `window` only ever needs the newest `want` rows, and an
// unfolded thinking block is not worth re-materializing per frame.
struct ScrollbackRows<'a> {
    rows: &'a [Row],
    /// The frame's width, for the rows that clip to fit.
    width: usize,
    /// For the folded summary row, which is synthesized at draw time and so
    /// carries no paint of its own.
    paint: &'a Paint,
    /// Next entry to read from the front, and the row offset inside it.
    front: (usize, usize),
    /// Next entry to read from the back, and the row offset inside it.
    back: (usize, usize),
}

impl<'a> ScrollbackRows<'a> {
    fn new(rows: &'a [Row], paint: &'a Paint, width: usize) -> Self {
        let back = rows.len().saturating_sub(1);
        let back_row = if rows.is_empty() { 0 } else { rows[back].len() };
        Self {
            rows,
            width,
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
        // front walk would index `rows[0]` before any check.
        if self.rows.is_empty() {
            return None;
        }
        while self.front.0 <= self.back.0 {
            let entry = &self.rows[self.front.0];
            if self.front.0 == self.back.0 {
                if self.front.1 >= self.back.1 {
                    return None;
                }
                let row = entry.line(self.front.1, self.paint, self.width);
                self.front.1 += 1;
                return Some(row);
            }
            if self.front.1 < entry.len() {
                let row = entry.line(self.front.1, self.paint, self.width);
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
            let entry = &self.rows[self.back.0];
            if self.front.0 == self.back.0 {
                if self.front.1 >= self.back.1 {
                    return None;
                }
                self.back.1 -= 1;
                return Some(entry.line(self.back.1, self.paint, self.width));
            }
            if self.back.1 > 0 {
                self.back.1 -= 1;
                return Some(entry.line(self.back.1, self.paint, self.width));
            }
            self.back = (self.back.0 - 1, self.rows[self.back.0 - 1].len());
        }
        None
    }
}

// The rows between the scrollback and the status line: the reasoning window,
// and the paragraph still being written.
//
// A free function because it is where both of this feature's bugs lived and
// `Ui` cannot be built without a terminal — a decision no test can reach is
// one that gets its second chance in front of the user.
fn body(
    thinking: &Thinking,
    scrollback: &[Row],
    md: &Markdown,
    reasoning: bool,
    partial: &str,
    space: (usize, usize),
    paint: &Paint,
) -> Vec<String> {
    let (width, room) = space;
    if thinking.holds(reasoning, scrollback) {
        // The block's count row in the scrollback already answers the fold
        // switch; the live placeholder is only for the moment before the
        // block's first completed line exists to count.
        let counted = thinking
            .streaming
            .is_some_and(|id| scrollback.iter().rev().any(|r| r.block() == Some(id)));
        if !counted {
            return vec![paint.on(&paint.theme.muted, THINKING)];
        }
        return Vec::new();
    }
    if partial.is_empty() {
        return Vec::new();
    }
    let painted = if reasoning {
        paint.on(&paint.theme.muted, partial)
    } else {
        md.line(partial, paint)
    };
    let mut rows = screen::fit(&painted, width);
    // A paragraph can outgrow the screen; the tail is the part still being
    // written, and the rest reaches scrollback when it closes.
    if rows.len() > room {
        rows.drain(..rows.len() - room);
    }
    rows
}

// A tool call still running, shown as one animated row in the live region
// until its result lands and the row scrolls up as a check or cross.
struct RunTool {
    id: String,
    name: String,
    summary: String,
}

// The one row a still-running tool occupies. The frame is the animation;
// `ToolEnd` and `abandon_tools` replace the row with a final line.
fn tool_row(frame: usize, name: &str, summary: &str) -> String {
    let frame = crate::status::FRAMES[frame % crate::status::FRAMES.len()];
    format!("{frame} {}", row::named(name, summary))
}

// The transcript as rows, exactly as the live stream would have drawn them:
// prompts with their gutter, answers as markdown, tool calls as their result
// lines, reasoning as a foldable block. A rewind rebuilds the screen from
// this, so the view returns to the point the conversation did.
fn scrollback_from(
    session: &agent::session::Session,
    paint: &Paint,
    prompt: &str,
    bang_prompt: &str,
    thinking: &mut Thinking,
) -> Vec<Row> {
    // A call whose result is in the session shows only its result row; one that
    // never got an answer (an interrupted turn) shows the start line instead,
    // the way `abandon_tools` leaves it.
    let answered: HashSet<String> = session
        .history()
        .filter_map(|e| match e {
            LogEntry::User {
                body: UserBody::Result { result: r, .. },
                ..
            } => Some(r.call.clone()),
            _ => None,
        })
        .collect();

    let mut out = Vec::new();
    // History, not the view: compaction is the model losing sight of the
    // conversation, not the user. What it dropped is marked and kept.
    let mut hidden = false;
    let unseen = session.out_of_view();
    for entry in session.history() {
        let gone = unseen.contains(&entry.id());
        if gone != hidden && !matches!(entry, LogEntry::Compaction { .. }) {
            hidden = gone;
            if gone {
                out.push(Row::notice(paint.on(
                    &paint.theme.muted,
                    "─── compacted; the model no longer sees the rest of this ───",
                )));
            }
        }
        match entry {
            LogEntry::User { body, .. } => match body {
                UserBody::Prompt(t) | UserBody::Aside(t) => {
                    out.extend(Row::prompt(t.shown_text(), prompt, bang_prompt, paint));
                }
                UserBody::Result { result: r, preview } => {
                    out.push(Row::stored_result(r, preview.as_deref()));
                }
                UserBody::Image(_) => {}
            },
            LogEntry::Assistant { blocks, .. } => {
                for b in blocks {
                    match b {
                        AssistantContent::Text(t) => {
                            // A fresh instance per block; where a block ends is
                            // the caller's to say, and `Row::answer` states the
                            // rest once for both callers.
                            let mut md = Markdown::default();
                            out.extend(Row::answer(&t.text, &mut md, paint));
                        }
                        AssistantContent::ToolCall(c) => {
                            if answered.contains(&c.id) {
                                continue;
                            }
                            out.push(Row::tool_start(&c.name, &render::summarize(&c.args), paint));
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
                                .flat_map(str::lines)
                                .map(|l| Row::reasoning_line(l, paint))
                                .collect();
                            // From the same counter the live stream draws
                            // from, because there is only one rule for what a
                            // block id is. Handing every rebuilt block `0`
                            // worked only for as long as nothing looked one up
                            // by id — and `streaming_row` and `stream_fold` both
                            // do, taking the last match.
                            out.push(Row::reasoning(thinking.take_id(), lines, thinking.folded));
                        }
                    }
                }
            }
            // Neither is anything the screen shows.
            LogEntry::Compaction { .. } => {}
        }
    }
    out
}

// Everything the terminal shows, and nothing the session knows.
/// What one lane looks like on screen: its conversation, the stream filling
/// it, where the view is parked and what this run has cost. Swapped whole when
/// the surface shows another lane; everything on `Ui` around it is the one
/// terminal, the one keyboard and whatever menu is open over them.
#[derive(Default)]
struct View {
    /// Model output with no newline after it yet. Kept live because it is still
    /// being written; a completed line goes straight to scrollback.
    partial: String,
    /// Whether `partial` is reasoning rather than the answer.
    reasoning: bool,
    /// Where the answer's markdown stands: what a row means depends on the
    /// rows before it, and only a fence carries that far.
    md: Markdown,
    thinking: Thinking,
    /// The conversation as the screen holds it: finished rows, oldest first,
    /// everything above the editor. Not a projection of the session — it also
    /// carries what only the screen ever knew, the banner and every notice a
    /// command or a warning left behind, interleaved where they happened.
    scrollback: Vec<Row>,
    /// Tool calls still running, one animated row each. A finished call
    /// replaces its row with the ✓/✗ line in scrollback, so a call that never
    /// answered would leave a spinning row behind; `abandon_tools` clears it.
    tools: Vec<RunTool>,
    /// Lines submitted while the run was working.
    queued: Vec<String>,
    /// Which worktree the session is working in, or None in the repository's
    /// own checkout. The status line says it, so the tree being edited is
    /// visible without asking.
    worktree: Option<String>,
    /// The instruction files named in the opening block, kept so `/reload`
    /// onto a new theme can rebuild it.
    context: Vec<String>,
    /// How many rows the opening block occupies. A theme change replaces
    /// exactly those and leaves the conversation under them alone.
    opened: usize,
    started: Option<Instant>,
    /// Whether this run has produced anything yet — a word, a thought, a call.
    /// Once it has, Esc means stop rather than unsend.
    committed: bool,
    /// Turns of this run that have already reported their totals.
    settled: Usage,
    /// The turn in flight, as far as the provider has said. Superseded rather
    /// than added to when its `TurnEnd` lands, or the input would count twice.
    turn: Usage,
    stopping: bool,
    /// Rows the view is scrolled up by. Zero shows the newest rows.
    scroll: usize,
    /// The last measurement of a scrolled-up view: item counts then, and the
    /// rows they wrapped to. A reflow in place (resize, fold-all) re-bases.
    counted: Option<(usize, usize, usize)>,
    /// What the last request occupied against what it may. From the loop, not
    /// measured here: this line repaints ten times a second.
    ctx: Option<(usize, usize)>,
    /// Shrinks so far this run.
    compactions: usize,
    /// The model in force. Copied in before the run borrows the agent, which
    /// is what puts it out of reach for the rest of the turn.
    model: String,
}

struct Ui {
    screen: Screen,
    keys: Arc<Keys>,
    editor: Editor,
    paint: Paint,
    /// The painted prompt gutter, shared by the editor and the echoed lines.
    prompt: String,
    /// The same gutter for a `!` line, where the bang takes the icon's place.
    bang_prompt: String,
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
    lists: Lists,
    /// The same copy, of the same list `/help` prints.
    commands: Arc<Vec<Command>>,
    /// The config paths `/settings` can reach, from `settings::leaves`.
    /// Rebuilt whenever the config tree is replaced.
    setting_paths: Vec<String>,
    /// The open settings panel, or None. While it is open it owns the menu
    /// rows and intercepts the menu keys before the editor does.
    settings: Option<settings::Panel>,
    /// When the last `ctrl+l` was pressed, for the new-session double-tap.
    last_l: Option<Instant>,
    last_interrupt: Option<Instant>,
    /// When the last Esc was pressed, for the rewind selector's double-tap.
    last_esc: Option<Instant>,
    /// The rewind selector's rows, session order, newest last. Empty is closed;
    /// while it is open it replaces the completion list in the same rows.
    rewind: Vec<MenuEntry>,
    spinner: usize,
    /// The segments each line shows, in the order the config named them.
    live: Vec<crate::status::Segment>,
    done: Vec<crate::status::Segment>,
    /// The lane in front. One for now; the surface shows one at a time
    /// either way.
    view: View,
}

// One row either menu can offer: a completion of the line, or a message
// from the rewind selector to go back to.
#[derive(Clone)]
enum MenuEntry {
    Completion(Candidate),
    /// `help` says who a row belongs to: two rows of prose read alike, and
    /// which one it is decides whether picking it unsends or continues from.
    Message {
        id: EntryId,
        show: String,
        help: &'static str,
    },
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
            MenuEntry::Message { help, .. } => help,
        }
    }
}

impl Ui {
    fn new(
        screen: Screen,
        keys: Arc<Keys>,
        choices: Vec<Choice>,
        commands: Arc<Vec<Command>>,
        lists: Lists,
        context: &[String],
        paint: Paint,
    ) -> Self {
        let opening = Row::banner(context, &paint);
        let opened = opening.len();
        let prompt = Self::paint_prompt(&paint, &paint.theme.prompt.icon);
        let bang_prompt = Self::paint_prompt(&paint, "!");
        let mut editor = Editor::default();
        editor.set_prompts(prompt.clone(), bang_prompt.clone());
        Self {
            screen,
            keys,
            choices,
            commands,
            lists,
            editor,
            paint,
            prompt,
            bang_prompt,
            last_l: None,
            picked: None,
            dismissed_at: None,
            last_interrupt: None,
            last_esc: None,
            rewind: Vec::new(),
            setting_paths: Vec::new(),
            settings: None,
            spinner: 0,
            live: crate::status::default_live(),
            done: crate::status::default_done(),
            // Everything else about a lane starts empty, which is what a lane
            // nobody has said anything in yet looks like.
            view: View {
                context: context.to_vec(),
                opened,
                scrollback: opening,
                ..View::default()
            },
        }
    }

    /// The values both lines draw on, as this surface currently knows them.
    fn snapshot(&self) -> crate::status::Snapshot<'_> {
        crate::status::Snapshot {
            elapsed: self.view.started.map(|s| s.elapsed()),
            input: self.view.settled.input + self.view.turn.input,
            output: self.view.settled.output + self.view.turn.output,
            cache_read: self.view.settled.cache_read + self.view.turn.cache_read,
            ctx: self.view.ctx,
            compactions: self.view.compactions,
            queued: self.view.queued.len(),
            model: &self.view.model,
            worktree: self.view.worktree.as_deref(),
            // Only a finished run states these, and it brings its own.
            cost: None,
            turns: None,
        }
    }

    /// The prompt gutter as the terminal shows it, colour and all.
    fn paint_prompt(paint: &Paint, icon: &str) -> String {
        format!("{} ", paint.on(&paint.theme.prompt.color, icon))
    }
    fn say(&mut self, line: impl Into<String>) {
        self.view.scrollback.push(Row::notice(line));
    }

    /// Where a finished row goes: a reasoning line into the streaming block's
    /// foldable entry, anything else straight into scrollback.
    fn land(&mut self, painted: String, reasoning: bool) {
        if reasoning && let Some(id) = self.view.thinking.streaming {
            if let Some(row) = self.streaming_row(id) {
                row.push_line(painted);
                return;
            }
            // The block's first line: born the way `ctrl+t` last left the last
            // block — its own fold, not the switch.
            self.view.scrollback.push(Row::reasoning(
                id,
                vec![painted],
                self.view.thinking.birth_fold(),
            ));
            return;
        }
        self.view.scrollback.push(Row::notice(painted));
    }

    /// The scrollback entry for a streaming block, if it has one yet.
    fn streaming_row(&mut self, id: u64) -> Option<&mut Row> {
        self.view.scrollback
            .iter_mut()
            .rev()
            .find(|r| r.block() == Some(id))
    }

    /// End the open paragraph and send it up into scrollback.
    /// A finished row, styled for what it is: reasoning, or the answer's
    /// markdown. The one place either decision is made.
    fn paint_row(&mut self, line: &str, reasoning: bool) -> String {
        if reasoning {
            return Row::reasoning_line(line, &self.paint);
        }
        Row::answer_line(line, &mut self.view.md, &self.paint)
    }

    fn close(&mut self) {
        if !self.view.partial.is_empty() {
            let text = std::mem::take(&mut self.view.partial);
            let painted = self.paint_row(&text, self.view.reasoning);
            self.land(painted, self.view.reasoning);
        }
        if self.view.reasoning {
            // The block is over: it stops taking lines; its entry is already
            // in the scrollback, folded or not.
            self.view.thinking.close_block();
        } else {
            // A fence the answer left open stays open only within the answer.
            // A tool call ends the block, and the block is as far as markdown
            // state can honestly reach.
            self.view.md.reset();
        }
        self.view.reasoning = false;
    }

    fn write(&mut self, delta: &str, reasoning: bool) {
        if reasoning != self.view.reasoning {
            self.close();
            self.view.reasoning = reasoning;
            if reasoning {
                // A new reasoning block: `close` just settled the previous
                // one; this one gets a fresh id and pushes the old last one
                // back to the switch.
                self.view.thinking.start(&mut self.view.scrollback);
            }
        }

        self.view.partial.push_str(delta);
        // A finished line is no longer changing, so it belongs in the
        // scrollback rather than in the region we repaint: reasoning into
        // the streaming block's foldable entry, answer text as a plain row.
        while let Some(i) = self.view.partial.find('\n') {
            let line: String = self.view.partial.drain(..=i).collect();
            let line = line.trim_end_matches('\n').to_string();
            let painted = self.paint_row(&line, reasoning);
            self.land(painted, reasoning);
        }
    }

    fn on_event(&mut self, event: Event) {
        // Anything the model produces spends the chance to unsend: past this
        // the prompt has been answered, not merely sent.
        if matches!(
            event,
            Event::TextDelta(_) | Event::ReasoningDelta(_) | Event::ToolStart { .. }
        ) {
            self.view.committed = true;
        }
        match &event {
            Event::TextDelta(d) => self.write(d, false),
            Event::ReasoningDelta(d) => self.write(d, true),
            Event::Usage(usage) => {
                // A retry sends a second one for the same turn: the count it
                // carries replaces the abandoned attempt's rather than joining
                // it.
                self.view.turn = *usage;
            }
            Event::TurnEnd { usage, .. } => {
                self.view.settled.input += usage.input;
                self.view.settled.output += usage.output;
                self.view.settled.cache_read += usage.cache_read;
                self.view.settled.cache_write += usage.cache_write;
                self.view.turn = Usage::default();
            }
            Event::TurnStart { .. } => {}
            Event::Context { used, budget } => self.view.ctx = Some((*used, *budget)),
            Event::Done { .. } => {
                self.close();
                if let Some(mut snap) = crate::status::Snapshot::of_done(&event) {
                    snap.model = &self.view.model;
                    snap.worktree = self.view.worktree.as_deref();
                    // Still running as far as the screen is concerned: `turn`
                    // clears this only once the loop returns.
                    snap.elapsed = self.view.started.map(|s| s.elapsed());
                    let line = crate::status::line(&self.done, &snap);
                    if !line.is_empty() {
                        let said = self.paint.on(&self.paint.theme.muted, &line);
                        self.view.scrollback.push(Row::notice(said));
                    }
                }
            }
            // A call's two events are one line here: the start takes a row in
            // the live region (where the spinner can animate it), and the end
            // scrolls that row up as its ✓/✗ line. Parallel calls each hold a
            // row, matched back by id because they end out of order.
            Event::ToolStart { id, name, args, .. } => {
                self.close();
                self.view.tools.push(RunTool {
                    id: id.clone(),
                    name: name.clone(),
                    summary: render::summarize(args),
                });
            }
            Event::ToolEnd {
                id,
                name,
                is_error,
                preview,
            } => {
                self.close();
                self.view.tools.retain(|t| t.id != *id);
                // The same row the rebuild produces, from the same parts. Two
                // renderings of this is what the second producer used to buy.
                self.view.scrollback
                    .push(Row::result(!is_error, name.clone(), preview.clone()));
            }
            _ => {
                if matches!(event, Event::Compacted(_)) {
                    self.view.compactions += 1;
                }
                self.close();
                if let Some(said) = render::describe(&event, &self.paint, self.screen.usable()) {
                    // Row by row: a scrollback line is written with a carriage
                    // return of its own, and an embedded newline would stair-
                    // step down the screen without one.
                    self.view.scrollback.extend(said.lines().map(Row::notice));
                }
            }
        }
    }

    /// A run that ended without answering a call leaves its animated row
    /// dangling. The call's own end event is never sent — a cancelled run
    /// returns before its results are reported — so give the scrollback the
    /// start line the row stood for and clear the row.
    fn abandon_tools(&mut self) {
        for t in std::mem::take(&mut self.view.tools) {
            let row = Row::tool_start(&t.name, &t.summary, &self.paint);
            self.view.scrollback.push(row);
        }
    }

    /// What the line could still become: a completion while a command word is
    /// being typed, or — with the rewind selector open — the user messages a
    /// conversation can be rewound to. Never during a run, when the editor is
    /// a queue, not a command line.
    fn menu(&self) -> Vec<MenuEntry> {
        if self.view.started.is_some() {
            return Vec::new();
        }
        if self.settings.is_some() {
            // The panel owns this space; the completion list waits.
            return Vec::new();
        }
        if !self.rewind.is_empty() {
            return self.rewind.clone();
        }
        if self.dismissed_at.as_deref() == Some(self.editor.text()) {
            return Vec::new();
        }
        // Bottom-up: the best match belongs on the row right above the input.
        repl::complete(
            self.editor.text(),
            &self.commands,
            &self.choices,
            self.lists.sessions(),
            &self.setting_paths,
            self.lists.worktrees(),
        )
        .into_iter()
        .rev()
        .map(MenuEntry::Completion)
        .collect()
    }

    /// Open the rewind selector on the given messages, newest selected first.
    fn open_rewind(&mut self, rows: Vec<MenuEntry>) {
        self.picked = Some(rows.len().saturating_sub(1));
        self.rewind = rows;
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
        let head = menu
            .iter()
            .map(|c| unicode_width::UnicodeWidthStr::width(c.show()))
            .max()
            .unwrap_or(0);
        let muted = self.rat_style(&self.paint.theme.muted);
        menu.iter()
            .map(|c| {
                let line = format!("  {}  {}", render::pad(c.show(), head), c.help());
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

        for t in &self.view.tools {
            let line = tool_row(self.spinner, &t.name, &t.summary);
            rows.extend(screen::fit(
                &self.paint.on(&self.paint.theme.muted, &line),
                width,
            ));
        }

        rows.extend(body(
            &self.view.thinking,
            &self.view.scrollback,
            &self.view.md,
            self.view.reasoning,
            &self.view.partial,
            (width, room),
            &self.paint,
        ));

        if self.view.started.is_some() {
            let mut parts = crate::status::parts(&self.live, &self.snapshot());
            // Not a segment: a run that can be stopped has to say so, and a
            // config that left it out would strand the user mid-turn.
            parts.push(
                if self.view.stopping {
                    "stopping…"
                } else {
                    "esc to stop"
                }
                .to_string(),
            );
            let spin = if self.view.stopping {
                "·"
            } else {
                crate::status::FRAMES[self.spinner % crate::status::FRAMES.len()]
            };
            let line = format!("{spin} {}", parts.join(" · "));
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
        // The opening block is painted once at construction; rebuild it so a
        // /reload lands on the new theme instead of the old.
        let opening = Row::banner(&self.view.context, &self.paint);
        let rest = self.view.scrollback.split_off(self.view.opened);
        self.view.opened = opening.len();
        self.view.scrollback = opening.into_iter().chain(rest).collect();
    }

    fn flush(&mut self) {
        let menu = self.menu();
        let width = self.screen.usable();
        let (input, caret) = self.editor.view(&self.paint, width);
        // A paste taller than the terminal must not push the editor area off
        // the bottom; the editor scrolls to keep the caret's row visible.
        let editor_h = input
            .len()
            .min((self.screen.height as usize).saturating_sub(1));
        let editor_top = (caret.0 as usize + 1).saturating_sub(editor_h);
        let input_view: Vec<String> = input.into_iter().skip(editor_top).take(editor_h).collect();
        let caret_in_view = (caret.0 as usize).saturating_sub(editor_top);
        // From the bottom up: the input line is pinned, the menu sits above
        // it, and the scrolled history fills what is left. The caret's row
        // therefore depends only on the pinned rows, never on how the
        // history wraps.
        let panel = self.settings.as_ref().map(|p| p.view(&self.paint, width));
        let panel_h = panel.as_ref().map(|v| v.len()).unwrap_or(0);
        let menu_h = if panel.is_some() {
            panel_h.min((self.screen.height as usize).saturating_sub(editor_h + 1))
        } else if menu.is_empty() {
            0
        } else {
            menu.len()
                .min((self.screen.height as usize).saturating_sub(editor_h + 1))
        };
        let hist_view = (self.screen.height as usize)
            .saturating_sub(editor_h + menu_h)
            .max(1);
        let live = self.live(hist_view);

        // While the view is scrolled up, rows the bottom gained since the
        // last measurement fold back into `scroll`, keeping the window put.
        if self.view.scroll > 0 {
            let items = (self.view.scrollback.len(), live.len());
            // A frame whose item counts match the last measurement has not
            // grown — nothing to fold, and no reason to re-wrap the history.
            if self.view.counted.is_none_or(|(sb, lv, _)| (sb, lv) != items) {
                let total = self.scrollback_rows(width) + live.len();
                self.view.scroll =
                    absorb_growth(self.view.scroll, self.view.counted.map(|(_, _, t)| t), total);
                self.view.counted = Some((items.0, items.1, total));
            }
        } else {
            self.view.counted = None;
        }
        // Measured in rows, not lines: a line wider than the terminal wraps
        // into several, and counting lines here would put more rows in the
        // area than fit — pushing the newest ones off the bottom, underneath
        // the input, where nothing shows them.
        let scrollback = ScrollbackRows::new(&self.view.scrollback, &self.paint, width);
        let (rows, scroll) = screen::window(
            scrollback.chain(live.iter().map(|s| Cow::Borrowed(s.as_str()))),
            width,
            hist_view,
            self.view.scroll,
        );

        self.view.scroll = scroll;
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
            if let Some(panel) = &panel {
                frame.render_widget(Rows(panel), menu_area);
            } else if !items.is_empty() {
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

    /// Rows the scrollback renders to at this width, wraps included.
    fn scrollback_rows(&self, width: usize) -> usize {
        ScrollbackRows::new(&self.view.scrollback, &self.paint, width)
            .map(|line| screen::fit(&line, width).len())
            .sum()
    }

    /// Rebuild the history from the transcript, forgetting everything the old
    /// drawing showed: a rewind changes what the conversation is, and the
    /// screen has to show the new one, not the old one with a note on it.
    fn rebuild(&mut self, session: &agent::session::Session) {
        self.view.scrollback.clear();
        // The opening block went with it; a rebuilt screen is the conversation.
        self.view.opened = 0;
        self.view.partial.clear();
        self.view.reasoning = false;
        self.view.tools.clear();
        self.view.thinking.streaming = None;
        self.view.thinking.last = self.view.thinking.folded;
        self.view.md.reset();
        self.view.scroll = 0;
        self.view.scrollback = scrollback_from(
            session,
            &self.paint,
            &self.prompt,
            &self.bang_prompt,
            &mut self.view.thinking,
        );
    }

    /// Accept a submitted input: echo it so the prompt survives the editor
    /// being cleared, then fold the block that was current back to the switch
    /// — the input pushes it out of current no matter what it turns out to be.
    fn submit(&mut self, line: &str) {
        let rows = Row::prompt(line, &self.prompt, &self.bang_prompt, &self.paint);
        self.view.scrollback.extend(rows);
        self.view.thinking.fold_previous(&mut self.view.scrollback);
    }

    fn key(&mut self, event: TermEvent, running: bool) -> Act {
        let key = match event {
            TermEvent::Resize(w, h) => {
                self.screen.resized(w, h);
                // Re-measuring starts at the new width: the re-wrap is a
                // change of layout, not output, and must not move the view.
                self.view.counted = None;
                return Act::None;
            }
            TermEvent::Paste(text) => {
                self.last_esc = None;
                self.editor.insert_str(&text.replace('\r', "\n"));
                return Act::None;
            }
            TermEvent::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_view(true, 1),
                    MouseEventKind::ScrollDown => self.scroll_view(false, 1),
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
        // The panel counts as a menu: its own keys are the Menu bindings, and
        // `menu()` is empty while it is open, so the layer has to be forced on.
        let bound = self
            .keys
            .action(press, self.settings.is_some() || !self.menu().is_empty(), running);

        // The settings panel owns the menu keys while it is open.
        if let Some(panel) = &mut self.settings {
            match bound {
                Some(Action::MenuNext) => {
                    panel.next();
                    return Act::None;
                }
                Some(Action::MenuPrevious) => {
                    panel.previous();
                    return Act::None;
                }
                Some(Action::MenuAccept) => {
                    if panel.editing() {
                        let (path, _) = panel.rows[panel.at].clone();
                        let value = panel.editing_value().to_string();
                        return Act::CommitSetting(path, value);
                    } else {
                        panel.begin_edit();
                        return Act::None;
                    }
                }
                Some(Action::MenuDismiss) => {
                    if panel.dismiss() {
                        self.settings = None;
                    }
                    return Act::None;
                }
                _ => {
                    // Printable keys edit the panel's value; everything else
                    // falls through to the normal editor and is ignored.
                    if panel.editing() {
                        if let KeyCode::Char(c) = key.code
                            && !key
                                .modifiers
                                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        {
                            panel.insert(c);
                        } else if matches!(bound, Some(Action::DeleteCharBack)) {
                            panel.backspace();
                        }
                        return Act::None;
                    }
                }
            }
        }

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
                        self.editor.take(true);
                        return if line.trim().is_empty() {
                            Act::None
                        } else {
                            Act::Submit(line)
                        };
                    }
                    None => {
                        // A secret value must not reach the recall history,
                        // which is written to disk in the clear.
                        let remember = !secret_settings_set(self.editor.text());
                        let typed = self.editor.take(remember);
                        return if typed.trim().is_empty() {
                            Act::None
                        } else {
                            Act::Submit(typed)
                        };
                    }
                }
            }
            Some(Action::RunInterrupt) => {
                // Esc before the model has moved means "I didn't mean to send
                // that"; an empty editor, or unsending overwrites a line.
                if self.editor.is_empty() && self.view.started.is_some() && !self.view.committed {
                    return Act::Unsend;
                }
                return Act::Interrupt;
            }
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
            Some(Action::AppClearScreen) => {
                // One press clears the screen; a second, inside the window,
                // starts a fresh session and rebuilds the screen empty.
                let now = Instant::now();
                if double_tap(&mut self.last_l, now) {
                    self.last_l = None;
                    return Act::NewSession;
                }
                self.screen.clear();
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
            Some(Action::ScrollPageUp) => self.scroll_view(true, self.page_scroll_step()),
            Some(Action::ScrollPageDown) => self.scroll_view(false, self.page_scroll_step()),
            Some(Action::ScrollHalfUp) => self.scroll_view(true, self.half_scroll_step()),
            Some(Action::ScrollHalfDown) => self.scroll_view(false, self.half_scroll_step()),
            Some(Action::ThinkFold) => {
                // The last block only: the one streaming, or the newest
                // finished one when nothing is. The switch is left alone, so
                // the blocks no one is touching keep what they had.
                self.view.thinking.toggle_current(&mut self.view.scrollback);
            }
            Some(Action::ThinkFoldAll) => {
                // Every block in the scrollback, the last one included, and
                // the switch with them: one key presses the whole screen to a
                // single state.
                self.view.thinking.flip_all(&mut self.view.scrollback);
                // A fold-all reflows blocks above the view too; re-baseline.
                self.view.counted = None;
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
                | Action::Rewind
                | Action::AppClearScreen,
            ) => unreachable!("handled scrollback"),
        }
        Act::None
    }

    /// Nudge the scrolled history window by `step` rows, up or down.
    fn scroll_view(&mut self, up: bool, step: usize) {
        self.view.scroll = if up {
            self.view.scroll.saturating_add(step)
        } else {
            self.view.scroll.saturating_sub(step)
        };
    }

    /// A page keeps 4 rows of context at the edge, the way the upstream pi
    /// TUI does (`Math.max(1, viewportHeight - 4)`).
    fn page_scroll_step(&self) -> usize {
        (self.screen.height as usize).saturating_sub(4).max(1)
    }

    fn half_scroll_step(&self) -> usize {
        ((self.screen.height as usize) / 2).max(1)
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
// Scroll for the same window one frame later: growth since `last` folds
// back into the offset so the window keeps its place; `None` re-bases.
fn absorb_growth(scroll: usize, last: Option<usize>, total: usize) -> usize {
    match last {
        Some(last) => scroll.saturating_add_signed(total as isize - last as isize),
        None => scroll,
    }
}

// What a `Step::Handled` leaves behind: its lines, and whatever the command
// changed under the surface. A free function because a run in flight lands
// them from inside its own borrow, where `self` is in pieces.
fn land_handled(ui: &mut Ui, core: &Repl, lines: Vec<String>) {
    ui.view.scrollback.extend(lines.into_iter().map(Row::notice));
    // The key map lives in two places; a reload has to reach both or the
    // screen keeps answering to the old bindings.
    if !Arc::ptr_eq(&ui.keys, &core.keys) {
        ui.keys = core.keys.clone();
    }
    // Likewise the completion list: /reload is allowed to define models — and
    // skills — the last one did not.
    ui.choices = core.choices();
    if ui.paint.theme.as_ref() != &core.config.theme {
        ui.set_theme(Arc::new(core.config.theme.clone()));
    }
    if !Arc::ptr_eq(&ui.commands, &core.commands) {
        ui.commands = core.commands.clone();
    }
    // The config tree changed under a reload; the `/settings` completion list
    // follows it.
    ui.setting_paths = crate::settings::leaves(&core.file)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
}

// A line submitted while the run works. What it may do is settled before it
// runs: `command` cannot be asked and then ignored, because asking is doing.
fn submitted(core: &mut Repl, ui: &mut Ui, totals: &Totals, line: String) {
    match crate::repl::fate_of(&line) {
        Fate::Refused(why) => ui.say(why.to_string()),
        Fate::Queued => ui.view.queued.push(line),
        Fate::Now => {
            ui.submit(&line);
            // `fate` admits only `Handled` here. Anything else has already had
            // its effect by now, so the line must not go back on the queue —
            // running it again is the one thing worse than not showing it.
            if let Step::Handled(said) = core.command(&line, totals) {
                land_handled(ui, core, said);
            }
        }
    }
}

pub struct Tui {
    core: Repl,
    ui: Ui,
    keys: UnboundedReceiver<TermEvent>,
    totals: Totals,
    bridge: crate::wechat::Bridge,
}

// crossterm reads blockingly, so the keyboard gets a thread of its own and
// reaches the loop as just another channel.
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

// Where recalled prompts are kept between sessions.
fn history_path() -> Option<std::path::PathBuf> {
    tools::state::dir().map(|d| d.join("history"))
}

// Enough to recall from without the file growing without bound.
const HISTORY_KEEP: usize = 1_000;

impl Tui {
    pub fn new(core: Repl, keys: Arc<Keys>, bridge: crate::wechat::Bridge) -> Result<Self> {
        let paint = Paint::with_theme(true, Arc::new(core.config.theme.clone()));
        let mut ui = Ui::new(
            Screen::new()?,
            keys,
            core.choices(),
            core.commands.clone(),
            Lists::new(
                core.store.clone(),
                core.lane.ctx.workspace.root().to_path_buf(),
            ),
            &core.lane.context,
            paint,
        );
        ui.setting_paths = crate::settings::leaves(&core.file)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        ui.view.worktree = core.lane.worktree.clone();
        ui.live = core.config.status.live.clone();
        ui.done = core.config.status.done.clone();
        ui.view.model = core.lane.agent.spec.model.clone();
        if let Some(prior) = history_path().and_then(|p| std::fs::read_to_string(p).ok()) {
            ui.editor.seed_history(editor::decode(&prior));
        }
        // A resumed session shows its transcript from the start: the whole
        // screen is rebuildable now, so there is no reason to hide it.
        if let Some(session) = &core.lane.session
            && !session.is_empty()
        {
            ui.rebuild(session);
        }
        Ok(Self {
            core,
            ui,
            keys: reader(),
            totals: Totals::default(),
            bridge,
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

    /// Sessions change on the commands that create, delete or switch them;
    /// refresh the copy the completion menu reads.
    /// A turn or a switch can change what `/resume` would list. Dropped
    /// rather than recomputed: whoever asks next pays, and most of the time
    /// nobody does.
    fn refresh_sessions(&mut self) {
        self.ui.lists.forget();
    }

    /// A `/new` or a `/resume` replaced the session — the transcript is the
    /// source of truth again — so the screen is rebuilt from the new one
    /// instead of keeping the old conversation up, and the completion list
    /// follows.
    fn land_swap(&mut self, said: Vec<String>) {
        if let Some(session) = &self.core.lane.session {
            self.ui.rebuild(session);
        }
        // `at` forgets both lists, so it stands in for `refresh_sessions`: a
        // swap that did not move repeats the root, and drops them either way.
        self.ui.lists.at(self.core.lane.ctx.workspace.root());
        self.ui.view.worktree = self.core.lane.worktree.clone();
        self.ui.view.scrollback.extend(said.into_iter().map(Row::notice));
    }

    pub async fn run(
        mut self,
        tx: UnboundedSender<Event>,
        mut rx: UnboundedReceiver<Event>,
    ) -> Result<()> {
        loop {
            self.ui.flush();
            let line = if self.ui.view.queued.is_empty() {
                tokio::select! {
                    key = self.keys.recv() => match key {
                        Some(key) => match self.ui.key(key, false) {
                            Act::Submit(line) => Some(line),
                            Act::Quit => break,
                            Act::OpenRewind => {
                                self.open_rewind();
                                None
                            }
                            Act::Rewind(id) => {
                                self.rewind_turn(id);
                                None
                            }
                            Act::CommitSetting(path, value) => {
                                match self.core.commit_file(&path, &value) {
                                    Ok(said) => {
                                        let rows = crate::settings::leaves(&self.core.file);
                                        self.ui.setting_paths =
                                            rows.iter().map(|(p, _)| p.clone()).collect();
                                        self.ui.view.scrollback
                                            .extend(said.into_iter().map(Row::notice));
                                        if let Some(panel) = &mut self.ui.settings {
                                            panel.refresh(rows);
                                            panel.finish_edit();
                                        }
                                    }
                                    Err(why) => {
                                        if let Some(panel) = &mut self.ui.settings {
                                            panel.refuse(why);
                                        }
                                    }
                                }
                                None
                            }
                            Act::NewSession => {
                                let Step::Swap(said) = self.core.command("/new", &self.totals)
                                else {
                                    unreachable!("ctrl+l twice reaches the /new branch");
                                };
                                self.land_swap(said);
                                None
                            }
                            Act::Interrupt | Act::Unsend | Act::None => None,
                        },
                        None => break,
                    },
                    msg = self.bridge.rx.recv() => match msg {
                        Some(crate::wechat::Inbound::Text { text }) => Some(text),
                        Some(crate::wechat::Inbound::Stop) => {
                            self.ui.say("nothing running to stop");
                            None
                        }
                        Some(crate::wechat::Inbound::Notice(text)) => {
                            self.ui.say(text);
                            None
                        }
                        None => None,
                    },
                }
            } else {
                // One at a time. Joined, a command and a prompt become one
                // line, and `parse` reads only its first word — so whichever
                // came first decided what both of them were.
                Some(self.ui.view.queued.remove(0))
            };
            let Some(line) = line else { continue; };
            self.ui.submit(&line);
            // A fresh turn starts at the newest row: a view scrolled up to
            // read would otherwise stream the run's output out of sight.
            self.ui.view.scroll = 0;
            // Written per line rather than on the way out: quitting with two
            // Ctrl-Cs skips every tidy exit path there is.
            self.save_history();
            // Bare `/settings` opens the panel instead of going through the
            // line command's read-only list.
            if matches!(crate::repl::parse(&line), Some(crate::repl::Cmd::Settings(rest)) if rest.is_empty())
            {
                let rows = crate::settings::leaves(&self.core.file);
                self.ui.settings = Some(settings::Panel::new(rows));
                continue;
            }
            match self.core.command(&line, &self.totals) {
                Step::Quit => break,
                Step::Bash(command) => {
                    // The command runs off the key loop so Esc can stop it,
                    // exactly as an agent turn can be interrupted.
                    let cancel = CancellationToken::new();
                    let lines = {
                        let Self { core, ui, keys, bridge, .. } = &mut self;
                        let run = core.bash(&command, cancel.clone());
                        tokio::pin!(run);
                        loop {
                            ui.flush();
                            tokio::select! {
                                done = &mut run => break done,
                                Some(key) = keys.recv() => match ui.key(key, true) {
                                    // A `!` command has no prompt to take back,
                                    // so unsending here is only a stop.
                                    Act::Interrupt | Act::Unsend => {
                                        cancel.cancel();
                                        ui.view.stopping = true;
                                    }
                                    Act::Submit(line) => ui.view.queued.push(line),
                                    // Nothing else can stop a command that will not stop.
                                    Act::Quit => {
                                        ui.screen.leave();
                                        std::process::exit(130)
                                    }
                                    // Esc means interrupt while the run is in flight.
                                    Act::OpenRewind | Act::Rewind(_) | Act::NewSession => {}
                                    Act::CommitSetting(..) => {}
                                    Act::None => {}
                                },
                                msg = bridge.rx.recv() => match msg {
                                    Some(crate::wechat::Inbound::Stop) => {
                                        cancel.cancel();
                                        ui.view.stopping = true;
                                    }
                                    Some(crate::wechat::Inbound::Text { text }) => {
                                        ui.view.queued.push(text);
                                    }
                                    Some(crate::wechat::Inbound::Notice(text)) => {
                                        ui.say(text);
                                    }
                                    None => {}
                                },
                            }
                        }
                    };
                    self.ui
                        .view
                        .scrollback
                        .extend(lines.into_iter().map(Row::notice));
                }
                Step::Swap(said) => self.land_swap(said),
                Step::Handled(lines) => land_handled(&mut self.ui, &self.core, lines),
                Step::Compact(focus) => {
                    // Long enough to want the spinner, so it borrows the run's.
                    // Committed with it: a compaction is work under way, not a
                    // message someone can still take back.
                    self.ui.view.started = Some(Instant::now());
                    self.ui.view.committed = true;
                    let done = self.core.compact_now(focus.as_deref()).await;
                    self.ui.view.started = None;
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
                            let held = self.core.lane.agent.kept_tokens().unwrap_or(0);
                            let now = self.core.tokens_now();
                            let why = format!(
                                "nothing to compact — {now} tokens, all inside the {held} \
                                 kept as working context"
                            );
                            self.ui
                                .say(self.ui.paint.on(&self.ui.paint.theme.muted, &why));
                        }
                    }
                }
                Step::Wechat(cmd) => {
                    let said = match cmd {
                        repl::WechatCmd::Status => self.bridge.status(),
                        repl::WechatCmd::On => match self.bridge.on().await {
                            Ok(said) => said,
                            Err(e) => {
                                self.ui.say(format!("wechat: {e:#}"));
                                Vec::new()
                            }
                        },
                        repl::WechatCmd::Off => self.bridge.off(),
                    };
                    self.ui.view.scrollback
                        .extend(said.into_iter().map(Row::notice));
                }
                // What was submitted while the run worked is taken up by the
                // top of this loop, one entry at a time and each read as what
                // it is. Draining it here instead meant everything queued
                // became the next prompt, whatever it had been typed as.
                Step::Prompt { send, typed } => self.turn(send, typed, &tx, &mut rx).await,
            }
        }
        self.save_history();
        Ok(())
    }

    /// Cut the transcript at an entry — chosen from the selector, or the
    /// prompt an Esc took back — and say what is left.
    fn rewind_turn(&mut self, id: EntryId) {
        match self.core.rewind_to(id) {
            Ok(Rewound::Nothing) => {
                self.ui.say(
                    self.ui
                        .paint
                        .on(&self.ui.paint.theme.muted, "nothing to rewind to"),
                );
            }
            Ok(outcome) => {
                // The transcript is the source of truth again: rebuild the
                // whole view from it, so the screen returns to the node the
                // conversation did instead of keeping the forgotten turns.
                // It clears anything said before it: hence the notice after.
                if let Some(session) = &self.core.lane.session {
                    self.ui.rebuild(session);
                }
                let said = match outcome {
                    // Unsent is half-typed, not gone: naming where the
                    // transcript ends would name the wrong thing.
                    // Cancelling takes long enough to type into, and a line
                    // started meanwhile is the newer intent.
                    Rewound::Unsent(_) if !self.ui.editor.is_empty() => {
                        "unsent — the line you were typing stands; Up recalls it".to_string()
                    }
                    Rewound::Unsent(text) => {
                        self.ui.editor.set_line(&text);
                        "unsent — the message is back in the editor".to_string()
                    }
                    Rewound::Kept | Rewound::Nothing => {
                        let at = self
                            .core
                            .lane
                            .session
                            .as_ref()
                            .and_then(|s| s.last_node())
                            .map(|n| render::clip(n.show(), 60))
                            .filter(|t| !t.is_empty())
                            .map(|t| format!(" — the transcript now ends at {t}"))
                            .unwrap_or_default();
                        format!("rewound{at}")
                    }
                };
                self.ui
                    .say(self.ui.paint.on(&self.ui.paint.theme.muted, &said));
            }
            Err(e) => {
                self.ui
                    .say(format!("warning: the transcript was not saved: {e}"));
            }
        }
    }

    /// Open the rewind selector on every point the conversation can go back
    /// to: what the user asked, and what the model answered.
    fn open_rewind(&mut self) {
        let rows: Vec<MenuEntry> = self
            .core
            .lane
            .session
            .as_ref()
            .map(|s| s.rewind_nodes())
            .unwrap_or_default()
            .into_iter()
            .map(|node| MenuEntry::Message {
                id: node.id(),
                show: render::clip(node.show(), 60),
                help: match node {
                    Node::Ask { .. } => "you — unsends it",
                    Node::Reply { .. } => "model — carries on from here",
                },
            })
            .collect();
        if rows.is_empty() {
            self.ui.say(
                self.ui
                    .paint
                    .on(&self.ui.paint.theme.muted, "nothing to rewind to"),
            );
            return;
        }
        self.ui.open_rewind(rows);
    }

    async fn turn(
        &mut self,
        prompt: String,
        typed: Option<String>,
        tx: &UnboundedSender<Event>,
        rx: &mut UnboundedReceiver<Event>,
    ) {
        // Lent to the run for the length of the turn. Only the loop that owns
        // it starts a turn, so it is never away when one begins.
        let Some(mut carried) = self.core.lane.session.take() else {
            return;
        };
        carried.send_prompt(prompt, typed);
        let cancel = CancellationToken::new();
        let ctx = self.core.lane.ctx.clone().with_cancel(cancel.clone());

        self.ui.view.started = Some(Instant::now());
        self.ui.view.committed = false;
        self.ui.view.stopping = false;
        // Per-run figures: a new prompt's status line must not start from the
        // previous run's totals.
        self.ui.view.settled = Usage::default();
        self.ui.view.turn = Usage::default();
        self.ui.view.compactions = 0;
        // Read while the agent is still reachable: the run borrows it for the
        // rest of this turn, and `/model` may have replaced it since the last.
        self.ui.view.model = self.core.lane.agent.spec.model.clone();

        // Set inside the run's borrow, acted on after it: unsending has to
        // touch the session, and the run is holding it.
        let mut unsend = false;
        // The run takes the session with it and hands it back at the end. That
        // is what frees the loop: nothing of the session is borrowed while the
        // turn works, so a command typed into it can be answered on the spot.
        let agent = self.core.lane.agent.clone();
        let joined = {
            let Self { core, ui, keys, bridge, totals } = self;
            let sent = tx.clone();
            let mut run = tokio::spawn(async move {
                let out = agent.run(&mut carried, &ctx, &sent).await;
                (carried, out)
            });
            let mut tick = tokio::time::interval(crate::status::SPIN);
            loop {
                ui.flush();
                tokio::select! {
                    done = &mut run => break done,
                    Some(event) = rx.recv() => {
                        bridge.observe(&event).await;
                        ui.on_event(event);
                    }
                    Some(key) = keys.recv() => match ui.key(key, true) {
                        Act::Interrupt => { cancel.cancel(); ui.view.stopping = true; }
                        Act::Unsend => {
                            cancel.cancel();
                            ui.view.stopping = true;
                            unsend = true;
                        }
                        Act::Submit(line) => submitted(core, ui, totals, line),
                        // Nothing else can stop a run that will not stop.
                        Act::Quit => {
                            ui.screen.leave();
                            std::process::exit(130)
                        }
                        // Esc means interrupt while the run is in flight.
                        Act::OpenRewind | Act::Rewind(_) | Act::NewSession => {}
                        Act::CommitSetting(..) => {}
                        Act::None => {}
                    },
                    msg = bridge.rx.recv() => match msg {
                        Some(crate::wechat::Inbound::Stop) => {
                            cancel.cancel();
                            ui.view.stopping = true;
                        }
                        Some(crate::wechat::Inbound::Text { text }) => {
                            submitted(core, ui, totals, text);
                        }
                        Some(crate::wechat::Inbound::Notice(text)) => {
                            ui.say(text);
                        }
                        None => {}
                    },
                    _ = tick.tick() => ui.spinner += 1,
                }
            }
        };

        // The task carried the transcript. Only a panic in it loses that copy,
        // and then the save below must not put the empty one in its place.
        let (recovered, out) = match joined {
            Ok((session, out)) => {
                self.core.lane.session = Some(session);
                (true, out)
            }
            // The task carried the whole transcript, not just this turn, and a
            // panic in it dropped that copy. The archive is the last good one;
            // carrying the empty stand-in forward would save it over the real
            // one at the end of the next turn, which loses the conversation
            // rather than the turn.
            Err(e) => match self.core.store.load(&self.core.lane.id) {
                Ok(stored) => {
                    let session = stored.into_session();
                    self.core.lane.ctx.set_todos(session.todos().to_vec());
                    self.core.lane.session = Some(session);
                    self.ui.say(format!(
                        "the run did not finish: {e} — back to the transcript as last saved"
                    ));
                    (true, Ok(Totals::default()))
                }
                // Nothing to go back to and nothing safe to write: what a panic
                // in the run did before it had a task to happen inside of.
                Err(why) => {
                    self.ui.screen.leave();
                    eprintln!("the run did not finish: {e}");
                    eprintln!("and the transcript could not be read back: {why}");
                    std::process::exit(70);
                }
            },
        };

        // Whatever the run posted on its way out still has to be shown.
        while let Ok(event) = rx.try_recv() {
            self.bridge.observe(&event).await;
            self.ui.on_event(event);
        }
        self.ui.close();
        // A cancelled run's calls got no `ToolEnd`; their animated rows have
        // to reach scrollback some other way before the next flush draws them
        // as a frozen spinner.
        self.ui.abandon_tools();
        self.ui.view.started = None;

        // Saved either way: an interrupted turn is exactly the one worth
        // keeping. Not when the transcript never came back, though — the empty
        // one standing in for it would land on top of what is on disk.
        if recovered
            && let Err(e) = self.core.save()
        {
            self.ui
                .say(format!("warning: the transcript was not saved: {e}"));
        }

        // The save put the running session on disk, the one `/resume` is
        // likeliest to want back; make the completion list see it.
        self.refresh_sessions();

        // A stopped or failed turn never got its `Done`; whatever text had
        // accumulated still has to reach the phone.
        let cancelled = matches!(&out, Err(AgentError::Cancelled));
        self.bridge.finish_turn(cancelled).await;
        match out {
            Ok(totals) => self.totals.merge(&totals),
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

        // Last: the rebuild clears the "stopped" this turn printed, and the
        // cancel was the mechanism here rather than news.
        if unsend
            && let Some(id) = self.core.lane.session.as_ref().and_then(|s| s.last_ask())
        {
            self.rewind_turn(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cow, Row, ScrollbackRows, Thinking, absorb_growth, body, scrollback_from,
        secret_settings_set, tool_row,
    };
    use crate::render::Markdown;
    use crate::render::Paint;
    use crate::tui::screen;

    #[test]
    fn a_secret_settings_set_is_detected_by_its_path() {
        assert!(secret_settings_set("/settings set api_key x"));
        assert!(secret_settings_set("/settings set models.flash.api_key x"));
        assert!(!secret_settings_set("/settings set models.flash.context_window 1000"));
        assert!(!secret_settings_set("/settings get api_key"));
        assert!(!secret_settings_set("/todo show"));
    }
    /// Both scrollback producers draw block ids from one counter. They used
    /// not to: a rebuilt block was always `0`, which held only while nothing
    /// looked one up — and `streaming_row` and `stream_fold` both do, taking the
    /// last match, so two blocks sharing a number is two blocks the lookup
    /// cannot tell apart.
    #[test]
    fn rebuilt_reasoning_blocks_get_ids_of_their_own() {
        use agent::session::Session;
        use brain::message::{AssistantContent, Reasoning, ReasoningContent};

        let mut s = Session::new();
        s.prompt("go");
        for n in 0..3 {
            s.push_assistant(vec![AssistantContent::Reasoning(Reasoning {
                id: None,
                content: vec![ReasoningContent::Text {
                    text: format!("thought {n}"),
                    signature: None,
                }],
                by: None,
            })]);
        }

        let mut thinking = Thinking::default();
        let rows = scrollback_from(&s, &Paint::new(false), "> ", "! ", &mut thinking);
        let ids: Vec<u64> = rows.iter().filter_map(Row::block).collect();
        assert_eq!(ids.len(), 3, "{} rows, {ids:?}", rows.len());
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 3, "two blocks share a number: {ids:?}");
        // And the counter moved, so a block streamed after the rebuild cannot
        // land on one of these.
        assert!(!ids.contains(&thinking.take_id()), "{ids:?}");
    }

    /// The screen opens with what this run is standing on. It used to be said
    /// as a startup note, which scrolled away; here it stays at the top, which
    /// is where "what is my agent obeying" belongs.
    #[test]
    fn the_opening_block_names_the_instruction_files() {
        let paint = Paint::new(false);
        let rows = Row::banner(&["~/.pi/Pi.md".into(), "AGENTS.md".into()], &paint);
        let shown: Vec<String> = ScrollbackRows::new(&rows, &paint, 80)
            .map(|r| r.to_string())
            .collect();
        // The version from the same place the banner reads it: spelled out
        // here, every release breaks a test about the context files.
        assert_eq!(
            shown,
            [
                concat!("π ", env!("CARGO_PKG_VERSION")),
                "context:",
                "- ~/.pi/Pi.md",
                "- AGENTS.md"
            ]
        );
    }

    /// Nothing loaded, nothing said — the common case is one personal file and
    /// a heading over an empty list is worse than no heading.
    #[test]
    fn an_opening_block_with_no_instruction_files_is_the_banner_alone() {
        let rows = Row::banner(&[], &Paint::new(false));
        assert_eq!(rows.len(), 1);
    }

    /// A closed reasoning block of id `id` and `n` lines in the scrollback.
    fn block(id: u64, n: usize, folded: bool) -> Row {
        Row::reasoning(id, (1..=n).map(|i| format!("line {i}")).collect(), folded)
    }

    /// What the screen shows for a run in the middle of reasoning.
    fn shown(t: &Thinking, partial: &str) -> Vec<String> {
        body(
            t,
            &[],
            &Markdown::default(),
            true,
            partial,
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
        let rows = [block(1, 2, true)];
        let paint = Paint::new(false);
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, 80).collect();
        assert_eq!(rows, vec!["thinking · 2 lines"]);
    }

    #[test]
    fn an_unfolded_entry_shows_its_lines() {
        let rows = [block(1, 2, false)];
        let paint = Paint::new(false);
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, 80).collect();
        assert_eq!(rows, vec!["line 1", "line 2"]);
    }

    /// The divergence this change removes. The live stream rendered an edit's
    /// sketch; the rebuild rendered the stored result's first line — so the
    /// same turn looked one way while it happened and another after a rewind.
    /// Now both build the same row from the same parts.
    #[test]
    fn the_live_row_and_the_rebuilt_row_are_the_same_row() {
        let sketched = "2 files, +3 -1\n  12 + added\n  13 - gone";
        // What the stream pushes when the tool ends.
        let live = [Row::result(true, "edit", sketched)];
        // What the rebuild pushes, reading the entry that same turn stored.
        let stored = brain::message::ToolResult::text("c1", "edit", "✓ edit [a.rs#TAG] …");
        let rebuilt = [Row::stored_result(&stored, Some(sketched))];

        let paint = Paint::new(false);
        let a: Vec<Cow<'_, str>> = ScrollbackRows::new(&live, &paint, 80).collect();
        let b: Vec<Cow<'_, str>> = ScrollbackRows::new(&rebuilt, &paint, 80).collect();
        assert_eq!(a, b);
        assert_eq!(a.len(), 3, "head plus the two diff rows");
    }

    /// The rows of one result are painted once per width and handed out one at
    /// a time, so a stale cache would show the narrow frame's clipping in the
    /// wide one — and only below the head row, where the single-row case
    /// cannot see it.
    #[test]
    fn every_row_of_a_result_is_repainted_when_the_window_changes() {
        let paint = Paint::new(false);
        let long = "x".repeat(200);
        let rows = [Row::result(true, "edit", format!("head\n  12 + {long}"))];

        let narrow: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, 40).collect();
        let wide: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, 160).collect();
        // And back again: widening must not be the only direction that repaints.
        let again: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, 40).collect();

        assert_eq!(narrow.len(), 2, "head plus the one diff row");
        assert!(
            wide[1].len() > narrow[1].len(),
            "{} vs {}",
            wide[1],
            narrow[1]
        );
        assert_eq!(narrow, again, "the narrow frame came back different");
    }

    /// A tool that sketched nothing has nothing to store, and the rebuild falls
    /// back to the first line of what it did store — which is what
    /// `ToolOutput::preview` falls back to on the live side.
    #[test]
    fn a_result_without_a_sketch_falls_back_to_its_first_line() {
        let stored = brain::message::ToolResult::text("c1", "read", "fn main() {}\nmore");
        let rows = [Row::stored_result(&stored, None)];
        let paint = Paint::new(false);
        let out: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, 80).collect();
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("fn main() {}"), "{}", out[0]);
        assert!(!out[0].contains("more"), "only the first line");
    }

    /// The bug this shape exists to fix: a result row used to be clipped when
    /// it landed and stored as the clipped string, so widening the terminal
    /// could never bring back what had been cut.
    #[test]
    fn widening_the_window_gives_back_what_was_clipped() {
        let paint = Paint::new(false);
        let rows = [Row::result(true, "read", "a".repeat(200))];
        let narrow: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, 40).collect();
        let wide: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, 160).collect();

        assert!(narrow[0].ends_with('…'), "{}", narrow[0]);
        assert!(
            wide[0].len() > narrow[0].len(),
            "the wider frame showed no more of it: {} vs {}",
            wide[0].len(),
            narrow[0].len()
        );
        // Both are one row, and both name the tool.
        assert!(narrow[0].contains("read") && wide[0].contains("read"));
    }

    /// A failure is not a result that happens to read badly.
    #[test]
    fn a_failed_result_is_marked_as_one() {
        let paint = Paint::new(false);
        let bad = [Row::result(false, "read", "gone")];
        let good = [Row::result(true, "read", "gone")];
        let bad: Vec<Cow<'_, str>> = ScrollbackRows::new(&bad, &paint, 80).collect();
        let good: Vec<Cow<'_, str>> = ScrollbackRows::new(&good, &paint, 80).collect();
        assert!(bad[0].starts_with('✗'), "{}", bad[0]);
        assert!(good[0].starts_with('✓'), "{}", good[0]);
    }

    #[test]
    fn a_plain_entry_is_itself() {
        let rows = [Row::notice("hello".to_string())];
        let paint = Paint::new(false);
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, 80).collect();
        assert_eq!(rows, vec!["hello"]);
    }

    #[test]
    fn an_empty_scrollback_iterates_to_nothing() {
        // The front walk used to index `rows[0]` before any check, so a
        // forward iteration over an empty scrollback panicked.
        let paint = Paint::new(false);
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(&[], &paint, 80).collect();
        assert!(rows.is_empty());
    }

    #[test]
    fn scrollback_rows_walk_from_both_ends() {
        let rows = vec![
            Row::notice("a".to_string()),
            block(1, 2, false),
            Row::notice("d".to_string()),
        ];
        let paint = Paint::new(false);
        let rows = ScrollbackRows::new(&rows, &paint, 80);
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
        let scrollback = [block(1, 1, true)];
        let rows = body(
            &t,
            &scrollback,
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
        let mut scrollback = vec![Row::reasoning(9, vec!["old".to_string()], false)];
        t.start(&mut scrollback);
        scrollback.push(Row::reasoning(1, vec!["new".to_string()], true));
        t.toggle_current(&mut scrollback);
        assert!(t.folded);
        assert!(scrollback[0].folded() == Some(true));
        assert!(scrollback[1].folded() == Some(false));
    }

    #[test]
    fn a_finished_block_keeps_its_fold_until_the_next_question() {
        // An unfold survives the answer — a finished block is still last —
        // and folds back to the switch the moment a new input is submitted.
        let mut t = Thinking::default();
        t.start(&mut []);
        let mut scrollback = vec![block(1, 1, t.birth_fold())];
        t.toggle_current(&mut scrollback);
        assert!(scrollback[0].folded() == Some(false));
        t.close_block();
        // Still last until the next question is asked.
        assert!(scrollback[0].folded() == Some(false));
        t.fold_previous(&mut scrollback);
        // The submitted question pushes it out of last: it folds to the
        // switch.
        assert!(scrollback[0].folded() == Some(true));
        assert!(!t.birth_fold());
    }
    #[test]
    fn a_finished_block_follows_a_global_unfold() {
        // The fold follows the switch both ways: a screen the global key
        // opened keeps its block open once the next question takes over.
        let mut t = Thinking {
            folded: false,
            ..Default::default()
        };
        t.start(&mut []);
        let mut scrollback = vec![block(1, 1, t.birth_fold())];
        t.close_block();
        t.fold_previous(&mut scrollback);
        assert!(scrollback[0].folded() == Some(false));
    }

    #[test]
    fn a_new_block_in_the_same_answer_folds_the_previous_and_inherits_the_flip() {
        // A second reasoning block in the same answer is the new last: the
        // first one folds back to the switch, and the second is born the way
        // `ctrl+t` left the last block.
        let mut t = Thinking::default();
        t.start(&mut []);
        let mut scrollback = vec![block(1, 1, t.birth_fold())];
        t.toggle_current(&mut scrollback);
        assert!(scrollback[0].folded() == Some(false));
        t.close_block();
        t.start(&mut scrollback);
        scrollback.push(block(2, 1, t.birth_fold()));
        assert!(scrollback[0].folded() == Some(true));
        assert!(scrollback[1].folded() == Some(false));
    }

    #[test]
    fn a_flip_before_the_first_line_lands_on_birth() {
        // `ctrl+t` on a block with no entry yet flips the last value, not the
        // switch: it outlives close_block, and it is not a one-shot.
        let mut t = Thinking::default();
        t.start(&mut []);
        let mut scrollback: Vec<Row> = Vec::new();
        t.toggle_current(&mut scrollback);
        assert!(t.folded, "the switch itself is not touched");
        assert!(!t.birth_fold());
        t.close_block();
        assert!(!t.birth_fold());
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
        let scrollback = [block(1, 1, false)];
        assert!(!t.holds(true, &scrollback));
    }

    #[test]
    fn a_global_flip_takes_the_current_block_with_it() {
        // The case that named the key: everything else unfolded, the current
        // block folded on its own. The global key folds the whole screen —
        // the current block keeps its fold, because the fold is where the
        // rest are going.
        let mut t = Thinking {
            folded: false,
            ..Default::default()
        };
        t.start(&mut []);
        let mut scrollback = vec![Row::reasoning(1, vec!["new".to_string()], true)];
        t.flip_all(&mut scrollback);
        assert!(t.folded);
        assert!(scrollback[0].folded() == Some(true));
    }

    #[test]
    fn flipping_every_block_moves_the_switch_with_them() {
        // The global key folds or unfolds every block, the current one
        // included, and moves the switch with them: rows and switch never
        // disagree, so the screen always folds back to a single state.
        let mut t = Thinking::default();
        t.start(&mut []);
        let mut scrollback = vec![block(1, 1, true)];
        t.toggle_current(&mut scrollback); // unfold the current block on its own
        t.close_block();
        t.flip_all(&mut scrollback); // global fold
        assert!(!t.folded);
        assert!(scrollback.iter().all(|e| e.folded() == Some(false)));
        // The switch moved with them, so the next block is born unfolded.
        assert!(!t.birth_fold());
        // And a second global press folds the whole screen back.
        t.flip_all(&mut scrollback);
        assert!(t.folded);
        assert!(scrollback.iter().all(|e| e.folded() == Some(true)));
    }

    #[test]
    fn refolding_hides_lines_the_reader_has_already_seen() {
        // The screen repaints from scrollback every frame, so lines already
        // shown are still the same entry: folding them takes them back.
        let mut entry = block(1, 2, false);
        let paint = Paint::new(false);
        let rows: Vec<Cow<'_, str>> =
            ScrollbackRows::new(std::slice::from_ref(&entry), &paint, 80).collect();
        assert_eq!(rows, vec!["line 1", "line 2"]);
        entry.set_folded(true);
        let rows: Vec<Cow<'_, str>> =
            ScrollbackRows::new(std::slice::from_ref(&entry), &paint, 80).collect();
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
        let mut scrollback = vec![block(1, 1, t.birth_fold())];
        assert!(scrollback[0].folded() == Some(false));
        t.close_block();

        // A tool call ends the block; the next thinking block is the new
        // last, born unfolded, and the first one folds back to the switch.
        t.start(&mut scrollback);
        scrollback.push(block(2, 1, t.birth_fold()));
        assert!(scrollback[0].folded() == Some(true));
        assert!(scrollback[1].folded() == Some(false));
    }

    /// One frame of the scrolled-up view: what the window shows, through the
    /// same growth absorption `flush` applies.
    fn frame(
        content: &[String],
        room: usize,
        scroll: &mut usize,
        last_total: &mut Option<usize>,
    ) -> Vec<String> {
        let total = content.len();
        *scroll = absorb_growth(*scroll, *last_total, total);
        let (rows, s) = screen::window(
            content.iter().map(|s| Cow::Borrowed(s.as_str())),
            80,
            room,
            *scroll,
        );
        *scroll = s;
        *last_total = Some(total);
        rows
    }

    #[test]
    fn a_scrolled_up_view_holds_until_the_user_scrolls_back() {
        // Scrolled two rows up from ten rows of history, rows 5-8 stay put
        // as output arrives below; only the user's own scroll moves them.
        let mut content: Vec<String> = (1..=10).map(|n| n.to_string()).collect();
        let (room, mut scroll, mut last_total) = (4usize, 0usize, None);
        scroll = scroll.saturating_add(2);
        let first = frame(&content, room, &mut scroll, &mut last_total);
        assert_eq!(first, vec!["5", "6", "7", "8"]);
        for n in 11..=15 {
            content.push(n.to_string());
            assert_eq!(
                frame(&content, room, &mut scroll, &mut last_total),
                first,
                "row {n} arriving moved the scrolled-up window"
            );
        }
        scroll = scroll.saturating_sub(1);
        assert_eq!(
            frame(&content, room, &mut scroll, &mut last_total),
            vec!["6", "7", "8", "9"]
        );
    }

    #[test]
    fn rows_gone_below_the_window_leave_the_view_put() {
        // Rows removed below the window shrink the tail; the negative delta
        // is absorbed like a positive one and the window stays put.
        let mut content: Vec<String> = (1..=10).map(|n| n.to_string()).collect();
        let (room, mut scroll, mut last_total) = (4usize, 0usize, None);
        scroll = scroll.saturating_add(2);
        assert_eq!(
            frame(&content, room, &mut scroll, &mut last_total),
            vec!["5", "6", "7", "8"]
        );
        for n in 11..=15 {
            content.push(n.to_string());
            frame(&content, room, &mut scroll, &mut last_total);
        }
        content.truncate(10);
        assert_eq!(
            frame(&content, room, &mut scroll, &mut last_total),
            vec!["5", "6", "7", "8"]
        );
    }
}
