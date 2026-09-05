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

use agent::session::{Entry as LogEntry, EntryId, Node, Session, UserBody};
use agent::{AgentError, Event, Totals};
use anyhow::Result;
use brain::message::{AssistantContent, ReasoningContent};
use crossterm::event::{Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use futures::FutureExt;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::journal;
use crate::keys::{Action, Keys, Mode, Press};
use crate::render::Style as ThemeStyle;
use crate::render::{self, Markdown, Paint};
use crate::lane::{Lane, Turn};
use crate::repl::{self, Candidate, Choice, Command, Fate, Intent, Repl, Rewound, Step};
use crate::session::ResumeChoice;
use editor::Editor;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style as RStyle;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};
use row::Row;
use screen::{Rows, Screen};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

// What a folded run shows instead of what it is thinking.
const THINKING: &str = "thinking...";

// How close two Ctrl-C presses must be to read as one deliberate quit.
//
// Borrowed from pi, which uses the same 500ms. A latching flag looks simpler
// and is wrong: clear one half-typed line, type another, clear that — and the
// second clear reads as the second half of a double-tap and quits.
const DOUBLE_TAP: std::time::Duration = std::time::Duration::from_millis(500);

// How long a flash stays on the bar row: long enough to read a short line
// without looking for it, short enough that a second try lands after it.
const FLASH: std::time::Duration = std::time::Duration::from_secs(3);

// One text each: three and two call sites had their own copy of these, and a
// reworded one would have drifted.
const NO_TRANSCRIPT: &str = "this checkout has no transcript — /new or /resume first";
const NOTHING_TO_REWIND: &str = "nothing to rewind to";

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
    match repl::read(line) {
        Intent::Settings(rest) => {
            let mut parts = rest.splitn(3, char::is_whitespace);
            matches!(parts.next(), Some("set"))
                && parts.next().is_some_and(|p| journal::secret(journal::leaf(p)))
        }
        _ => false,
    }
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
    /// What a finished run's row spells itself out with, for the same reason:
    /// it is rendered here, not when the run ended.
    done: &'a [crate::status::Segment],
    /// Next entry to read from the front, and the row offset inside it.
    front: (usize, usize),
    /// Next entry to read from the back, and the row offset inside it.
    back: (usize, usize),
}

impl<'a> ScrollbackRows<'a> {
    fn new(
        rows: &'a [Row],
        paint: &'a Paint,
        done: &'a [crate::status::Segment],
        width: usize,
    ) -> Self {
        let back = rows.len().saturating_sub(1);
        let back_row = if rows.is_empty() { 0 } else { rows[back].len() };
        Self {
            rows,
            width,
            paint,
            done,
            front: (0, 0),
            back: (back, back_row),
        }
    }
}

impl<'a> Iterator for ScrollbackRows<'a> {
    type Item = (Cow<'a, str>, Option<&'a str>);

    fn next(&mut self) -> Option<Self::Item> {
        // Both walks index before they compare their pointers, and on an empty
        // scrollback `rows[0]` is already out of bounds.
        if self.rows.is_empty() {
            return None;
        }
        while self.front.0 <= self.back.0 {
            let entry = &self.rows[self.front.0];
            if self.front.0 == self.back.0 {
                if self.front.1 >= self.back.1 {
                    return None;
                }
                let item = entry.line(self.front.1, self.paint, self.done, self.width);
                self.front.1 += 1;
                return Some(item);
            }
            if self.front.1 < entry.len() {
                let item = entry.line(self.front.1, self.paint, self.done, self.width);
                self.front.1 += 1;
                return Some(item);
            }
            self.front = (self.front.0 + 1, 0);
        }
        None
    }
}

impl<'a> DoubleEndedIterator for ScrollbackRows<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.rows.is_empty() {
            return None;
        }
        while self.front.0 <= self.back.0 {
            let entry = &self.rows[self.back.0];
            if self.front.0 == self.back.0 {
                if self.front.1 >= self.back.1 {
                    return None;
                }
                self.back.1 -= 1;
                return Some(entry.line(self.back.1, self.paint, self.done, self.width));
            }
            if self.back.1 > 0 {
                self.back.1 -= 1;
                return Some(entry.line(self.back.1, self.paint, self.done, self.width));
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
                    out.extend(Row::prompt(t.shown_text(), bang_prompt, paint));
                }
                // Machine prose, not the user's line: rebuilt in the muted
                // voice of a screen notice rather than under the prompt
                // gutter.
                UserBody::Note(t) => {
                    for line in t.shown_text().lines() {
                        out.push(Row::notice(paint.on(&paint.theme.muted, line)));
                    }
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
/// it, where it is scrolled and what this run has cost. Owned by the lane it
/// belongs to, so the surface draws whichever `Repl::current` names and there
/// is no second list to keep in step; everything on `Ui` around it is the one
/// terminal, the one keyboard and whatever menu is open over them.
#[derive(Default)]
pub struct View {
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
    /// What arrived while the run was working, kept as intents rather than
    /// lines: their fate was settled at the door, and re-reading them on the
    /// way out would ask a question that has already been answered.
    queued: Vec<Intent>,
    /// Whether this lane's opening block has been built. A rebuild drops the
    /// banner along with everything else, so neither an empty scrollback nor a
    /// zero `opened` can stand in for "never drawn" — and drawing it a second
    /// time would stack two banners on one lane.
    drawn: bool,
    /// How many rows the opening block occupies. A theme change replaces
    /// exactly those and leaves the conversation under them alone.
    opened: usize,
    /// When the work in flight began, for the segment that times it. A clock
    /// and nothing else: whether a run is on is `Lane::turn`'s to say, and one
    /// field answering both left every ending path to put the clock back or
    /// leave a spinner running over a finished lane.
    started: Option<Instant>,
    /// Whether this run has produced anything yet — a word, a thought, a call.
    /// Once it has, Esc means stop rather than unsend.
    committed: bool,
    /// Every number this run has reported, as the events stated them. Both
    /// status lines read it, so the line the run ends on is the live line's
    /// last frame rather than a second count of the same turns.
    tally: crate::status::Tally,
    stopping: bool,
    /// Rows the view is scrolled up by. Zero shows the newest rows.
    scroll: usize,
    /// The last measurement of a scrolled-up view: item counts then, and the
    /// rows they wrapped to. A reflow in place (resize, fold-all) re-bases.
    counted: Option<(usize, usize, usize)>,
    /// The model in force. Copied in before the run borrows the agent, which
    /// is what puts it out of reach for the rest of the turn.
    model: String,
}

/// What a typed character means to the modal keys.
enum Typed {
    /// It lands in the line, as it would with vim keys off.
    Insert,
    /// It closed the escape sequence: the half already on screen has to come
    /// back off, and the mode has changed.
    Escape,
    /// Normal mode. An unbound character commands nothing and types nothing —
    /// without this the mode would be a costume, every key still typing.
    Ignore,
}

/// The modal keys' whole state: the mode that is up, the sequence that leaves
/// Insert, and the character that may be its first half.
///
/// One struct rather than four fields on `Ui`: none of them means anything
/// without the others, and `Ui` already carries more loose state than it
/// should. `Ui` holds it as an `Option`, so vim being off is the absence of
/// the state rather than a flag beside it — "off, but in Normal" cannot be
/// written down.
struct Vim {
    mode: Mode,
    /// The two characters that leave Insert, resolved once. `None` — an empty
    /// setting, or any other length — is no sequence, and with it no way into
    /// Normal at all.
    escape: Option<(char, char)>,
    window: std::time::Duration,
    /// The last character typed, and when. Lazy, like the double-taps: the
    /// character is on screen already and nothing is held pending, so the line
    /// is never a guess about a key that has not arrived.
    last: Option<(char, Instant)>,
}

impl Vim {
    fn new(cfg: &crate::config::Vim) -> Self {
        let mut vim = Self {
            mode: Mode::Insert,
            escape: None,
            window: std::time::Duration::ZERO,
            last: None,
        };
        vim.configure(cfg);
        vim
    }

    /// Take what the config says about the sequence, resolving the two
    /// characters here rather than at every keystroke. Anything that is not
    /// exactly two of them is no sequence — the documented way to leave
    /// Normal unreachable while keeping the layer's bindings listed.
    fn configure(&mut self, cfg: &crate::config::Vim) {
        let mut chars = cfg.escape.chars();
        self.escape = match (chars.next(), chars.next(), chars.next()) {
            (Some(a), Some(b), None) => Some((a, b)),
            _ => None,
        };
        self.window = std::time::Duration::from_millis(cfg.escape_timeout_ms);
    }

    /// What `c` does, and the mode change if it makes one.
    fn typed(&mut self, c: char, now: Instant) -> Typed {
        if self.mode == Mode::Normal {
            return Typed::Ignore;
        }
        let Some((first, second)) = self.escape else {
            return Typed::Insert;
        };
        let armed = self
            .last
            .take()
            .is_some_and(|(p, at)| p == first && now.duration_since(at) < self.window);
        if armed && c == second {
            self.mode = Mode::Normal;
            return Typed::Escape;
        }
        self.last = Some((c, now));
        Typed::Insert
    }
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
    /// The lane bar's separator, painted once beside the two above it: the bar
    /// is rebuilt every frame and this depends only on the theme.
    tab_sep: String,
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
    /// The modal keys, or None while they are off.
    vim: Option<Vim>,
    /// The segments each line shows, in the order the config named them.
    live: Vec<crate::status::Segment>,
    done: Vec<crate::status::Segment>,
    /// What the lane strip says, in lane order. Empty until there is a second
    /// lane, and the strip is absent with it — though a flash can still take
    /// that row.
    tabs: Vec<Tab>,
    /// A note answering the last keypress, painted, and when it landed. It
    /// takes the bar's row for `FLASH` and then goes — see `flash`.
    flash: Option<(String, Instant)>,
}


/// What to call a checkout. The root answers to its directory name, as
/// `worktree list` already names it — a fixed word would collide with a
/// checkout that happens to be called that.
fn lane_name(lane: &crate::lane::Lane) -> String {
    lane.worktree.clone().unwrap_or_else(|| {
        lane.ctx
            .workspace
            .root()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    })
}

/// How a lane shows in the bottom bar.
///
/// `Done`/`Failed` are unread marks, not history: the tool rows' ✓ stays for
/// good, this one goes the moment you look at the lane it belongs to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Front,
    Running,
    Done,
    Failed,
    Idle,
}

/// One lane as the bar shows it, rebuilt before every draw — the bar is a view
/// of state the surface does not own.
struct Tab {
    mark: Mark,
    name: String,
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

impl View {
    /// A lane nobody has said anything in yet: the banner naming what it
    /// stands on, and everything else empty.
    pub fn opening(context: &[String], paint: &Paint) -> Self {
        let scrollback = Row::banner(context, paint);
        Self {
            drawn: true,
            opened: scrollback.len(),
            scrollback,
            ..Self::default()
        }
    }
}

impl Ui {
    /// The draft belongs to the lane it was typed at, and the editor is the
    /// surface's rather than any view's — so a switch has to drop it, or the
    /// next Enter files it in whatever lane it landed on.
    /// What does not follow the surface to the next checkout: each was built
    /// against the lane being left, and the selector's rows are entries of
    /// that lane's transcript. The keys close it before any switch they can
    /// reach — a `/worktree` off the phone never passes through them.
    fn leave_lane(&mut self) {
        self.editor.take(false);
        self.flash = None;
        self.rewind.clear();
    }

    fn new(
        screen: Screen,
        keys: Arc<Keys>,
        choices: Vec<Choice>,
        commands: Arc<Vec<Command>>,
        lists: Lists,
        paint: Paint,
    ) -> Self {
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
            tab_sep: Self::paint_sep(&paint),
            paint,
            prompt,
            bang_prompt,
            last_l: None,
            picked: None,
            dismissed_at: None,
            last_interrupt: None,
            last_esc: None,
            rewind: Vec::new(),
            vim: None,
            setting_paths: Vec::new(),
            settings: None,
            spinner: 0,
            live: crate::status::default_live(),
            done: crate::status::default_done(),
            tabs: Vec::new(),
            flash: None,
        }
    }

    /// The values both lines draw on, as this surface currently knows them.
    fn snapshot(&self, lane: &Lane) -> crate::status::Snapshot {
        lane.view.tally.snapshot(
            &lane.view.model,
            lane.worktree.as_deref(),
            lane.view.started.map(|s| s.elapsed()),
            lane.view.queued.len(),
        )
    }

    /// The separator between lanes on the bar: the one every other line on
    /// this surface uses, dimmed so the names it divides are what the eye
    /// lands on.
    fn paint_sep(paint: &Paint) -> String {
        paint.on(&paint.theme.muted, " · ")
    }

    /// The prompt gutter as the terminal shows it, colour and all.
    fn paint_prompt(paint: &Paint, icon: &str) -> String {
        format!("{} ", paint.on(&paint.theme.prompt.color, icon))
    }
    fn say(&mut self, view: &mut View, line: impl Into<String>) {
        let line = line.into();
        // A backstop: what repeats most is a refusal, and those go to `flash`.
        if let Some(last) = view.scrollback.last_mut()
            && last.repeated(&line)
        {
            return;
        }
        view.scrollback.push(Row::notice(line));
    }

    /// Answer one keypress on the bar row and leave nothing behind.
    ///
    /// The scrollback is a transcript, and what a press did *not* do is not
    /// part of one — sent there it also stacked a row per press, which is how
    /// holding `ctrl+o` in a single checkout wrote a screenful of one line.
    /// Muted here rather than at the callers, which had drifted apart on it.
    fn flash(&mut self, line: impl Into<String>) {
        let text = self.paint.on(&self.paint.theme.muted, &line.into());
        self.flash = Some((text, Instant::now()));
    }

    /// The bar row while a flash is up, and the only place an expired one is
    /// dropped — every frame passes through here, so nothing else has to
    /// remember to clear it.
    fn flash_line(&mut self, width: usize) -> Option<String> {
        if self.flash.as_ref().is_some_and(|(_, at)| at.elapsed() >= FLASH) {
            self.flash = None;
        }
        let (text, _) = self.flash.as_ref()?;
        Some(render::clip(text, width))
    }

    /// Where a finished row goes: a reasoning line into the streaming block's
    /// foldable entry, anything else straight into scrollback.
    fn land(&mut self, view: &mut View, painted: String, reasoning: bool) {
        if reasoning && let Some(id) = view.thinking.streaming {
            if let Some(row) = self.streaming_row(view, id) {
                row.push_line(painted);
                return;
            }
            // The block's first line: born the way `ctrl+t` last left the last
            // block — its own fold, not the switch.
            view.scrollback.push(Row::reasoning(
                id,
                vec![painted],
                view.thinking.birth_fold(),
            ));
            return;
        }
        view.scrollback.push(Row::notice(painted));
    }

    /// The scrollback entry for a streaming block, if it has one yet.
    fn streaming_row<'a>(&mut self, view: &'a mut View, id: u64) -> Option<&'a mut Row> {
        view.scrollback
            .iter_mut()
            .rev()
            .find(|r| r.block() == Some(id))
    }

    /// End the open paragraph and send it up into scrollback.
    /// A finished row, styled for what it is: reasoning, or the answer's
    /// markdown. The one place either decision is made.
    fn paint_row(&mut self, view: &mut View, line: &str, reasoning: bool) -> String {
        if reasoning {
            return Row::reasoning_line(line, &self.paint);
        }
        Row::answer_line(line, &mut view.md, &self.paint)
    }

    fn close(&mut self, view: &mut View) {
        if !view.partial.is_empty() {
            let text = std::mem::take(&mut view.partial);
            let painted = self.paint_row(view, &text, view.reasoning);
            self.land(view, painted, view.reasoning);
        }
        if view.reasoning {
            // The block is over: it stops taking lines; its entry is already
            // in the scrollback, folded or not.
            view.thinking.close_block();
        } else {
            // A fence the answer left open stays open only within the answer.
            // A tool call ends the block, and the block is as far as markdown
            // state can honestly reach.
            view.md.reset();
        }
        view.reasoning = false;
    }

    fn write(&mut self, view: &mut View, delta: &str, reasoning: bool) {
        if reasoning != view.reasoning {
            self.close(view);
            view.reasoning = reasoning;
            if reasoning {
                // A new reasoning block: `close` just settled the previous
                // one; this one gets a fresh id and pushes the old last one
                // back to the switch.
                view.thinking.start(&mut view.scrollback);
            }
        }

        view.partial.push_str(delta);
        // A finished line is no longer changing, so it belongs in the
        // scrollback rather than in the region we repaint: reasoning into
        // the streaming block's foldable entry, answer text as a plain row.
        while let Some(i) = view.partial.find('\n') {
            let line: String = view.partial.drain(..=i).collect();
            let line = line.trim_end_matches('\n').to_string();
            let painted = self.paint_row(view, &line, reasoning);
            self.land(view, painted, reasoning);
        }
    }

    fn on_event(&mut self, lane: &mut Lane, event: Event) {
        // Anything the model produces spends the chance to unsend: past this
        // the prompt has been answered, not merely sent.
        if matches!(
            event,
            Event::TextDelta(_) | Event::ReasoningDelta(_) | Event::ToolStart { .. }
        ) {
            lane.view.committed = true;
        }
        // Every number either status line shows is read here, once. The arms
        // below decide only what reaches the scrollback.
        lane.view.tally.on(&event);
        match &event {
            Event::TextDelta(d) => self.write(&mut lane.view, d, false),
            Event::ReasoningDelta(d) => self.write(&mut lane.view, d, true),
            // Counted already, and none of them draws a row of its own.
            Event::Usage(_)
            | Event::TurnEnd { .. }
            | Event::TurnStart { .. }
            | Event::Context { .. } => {}
            Event::Done { .. } => {
                self.close(&mut lane.view);
                // Still running as far as the screen is concerned: `turn`
                // clears the clock only once the loop returns.
                let snap = self.snapshot(lane);
                // Asked now rather than at every draw: a run whose segments
                // all had nothing to say leaves no row, and a blank one is
                // worse than none.
                if !crate::status::parts(&self.done, &snap).is_empty() {
                    lane.view.scrollback.push(Row::tally(snap));
                }
            }
            // A call's two events are one line here: the start takes a row in
            // the live region (where the spinner can animate it), and the end
            // scrolls that row up as its ✓/✗ line. Parallel calls each hold a
            // row, matched back by id because they end out of order.
            Event::ToolStart { id, name, args, .. } => {
                self.close(&mut lane.view);
                lane.view.tools.push(RunTool {
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
                self.close(&mut lane.view);
                lane.view.tools.retain(|t| t.id != *id);
                // The same row the rebuild produces, from the same parts. Two
                // renderings of this is what the second producer used to buy.
                lane.view.scrollback
                    .push(Row::result(!is_error, name.clone(), preview.clone()));
            }
            _ => {
                self.close(&mut lane.view);
                if let Some(said) = render::describe(&event, &self.paint, self.screen.usable()) {
                    // Row by row: a scrollback line is written with a carriage
                    // return of its own, and an embedded newline would stair-
                    // step down the screen without one.
                    lane.view.scrollback.extend(said.lines().map(Row::notice));
                }
            }
        }
    }

    /// A run that ended without answering a call leaves its animated row
    /// dangling. The call's own end event is never sent — a cancelled run
    /// returns before its results are reported — so give the scrollback the
    /// start line the row stood for and clear the row.
    fn abandon_tools(&mut self, view: &mut View) {
        for t in std::mem::take(&mut view.tools) {
            let row = Row::tool_start(&t.name, &t.summary, &self.paint);
            view.scrollback.push(row);
        }
    }

    /// What the line could still become: a completion while a command word is
    /// being typed, or — with the rewind selector open — the user messages a
    /// conversation can be rewound to. Never during a run, when the editor is
    /// a queue, not a command line.
    fn menu(&self, running: bool) -> Vec<MenuEntry> {
        if running {
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
    fn highlighted(&self, running: bool) -> Option<MenuEntry> {
        let mut menu = self.menu(running);
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
    fn live(&self, lane: &Lane, room: usize) -> Vec<String> {
        let width = self.screen.usable();
        let mut rows = Vec::new();

        for t in &lane.view.tools {
            let line = tool_row(self.spinner, &t.name, &t.summary);
            rows.extend(screen::fit(
                &self.paint.on(&self.paint.theme.muted, &line),
                width,
            ));
        }

        rows.extend(body(
            &lane.view.thinking,
            &lane.view.scrollback,
            &lane.view.md,
            lane.view.reasoning,
            &lane.view.partial,
            (width, room),
            &self.paint,
        ));

        if lane.is_running() {
            let mut parts = crate::status::parts(&self.live, &self.snapshot(lane));
            // Not a segment: a run that can be stopped has to say so, and a
            // config that left it out would strand the user mid-turn.
            parts.push(
                if lane.view.stopping {
                    "stopping…"
                } else {
                    "esc to stop"
                }
                .to_string(),
            );
            let spin = if lane.view.stopping {
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

    /// The bottom bar, or None when there is nothing it could say. One lane is
    /// the whole surface, and a bar naming it is a row spent on nothing.
    fn lane_bar(&self, width: usize) -> Option<String> {
        if self.tabs.len() < 2 {
            return None;
        }
        let spin = crate::status::FRAMES[self.spinner % crate::status::FRAMES.len()];
        let theme = &self.paint.theme;
        let painted: Vec<String> = self
            .tabs
            .iter()
            .map(|tab| {
                // The sign says what a lane is doing; being in front is not
                // that, and `›` is the input prompt's. Plain against dim is
                // all it takes, on a row nothing should look at twice.
                let (sign, style) = match tab.mark {
                    Mark::Front => ("", &theme.input),
                    Mark::Running => (spin, &theme.muted),
                    Mark::Done => ("✓", &theme.status.ok),
                    Mark::Failed => ("✗", &theme.status.err),
                    Mark::Idle => ("", &theme.muted),
                };
                let label = if sign.is_empty() {
                    tab.name.clone()
                } else {
                    format!("{sign} {}", tab.name)
                };
                self.paint.on(style, &label)
            })
            .collect();
        Some(render::clip(&painted.join(&self.tab_sep), width))
    }

    /// The checkout after this one, wrapping at the end — what `ctrl+o` goes
    /// to. Every checkout on disk is in the ring, not only the ones already
    /// open: `Intent::Worktree` opens one that is not, which is the same thing
    /// the picker did when you chose an unopened row.
    ///
    /// None when there is nowhere else to go.
    fn next_checkout(&self, lane: &Lane) -> Option<String> {
        let trees = self.lists.worktrees();
        if trees.len() < 2 {
            return None;
        }
        // `worktree::list` puts the main checkout first and names it for its
        // directory, where a lane in it carries no worktree name at all.
        let at = match lane.worktree.as_deref() {
            Some(name) => trees.iter().position(|c| c.name == name).unwrap_or(0),
            None => 0,
        };
        Some(trees[(at + 1) % trees.len()].name.clone())
    }

    fn set_theme(&mut self, view: &mut View, context: &[String], theme: Arc<render::Theme>) {
        self.paint.theme = theme;
        self.bang_prompt = Self::paint_prompt(&self.paint, "!");
        self.tab_sep = Self::paint_sep(&self.paint);
        self.show_mode();
        // The opening block is painted once at construction; rebuild it so a
        // /reload lands on the new theme instead of the old.
        let opening = Row::banner(context, &self.paint);
        let rest = view.scrollback.split_off(view.opened);
        view.opened = opening.len();
        view.scrollback = opening.into_iter().chain(rest).collect();
    }

    fn flush(&mut self, lane: &mut Lane) {
        let menu = self.menu(lane.is_running());
        let width = self.screen.usable();
        // A flash outranks the lane strip: it is gone in a moment, where the
        // strip is always a keystroke away.
        let bar = self.flash_line(width).or_else(|| self.lane_bar(width));
        let bar_h = usize::from(bar.is_some());
        let (input, caret) = self.editor.view(&self.paint, width);
        // A paste taller than the terminal must not push the editor area off
        // the bottom; the editor scrolls to keep the caret's row visible.
        let editor_h = input
            .len()
            .min((self.screen.height as usize).saturating_sub(1 + bar_h));
        let editor_top = (caret.0 as usize + 1).saturating_sub(editor_h);
        let input_view: Vec<String> = input.into_iter().skip(editor_top).take(editor_h).collect();
        let caret_in_view = (caret.0 as usize).saturating_sub(editor_top);
        // From the bottom up: the input line is pinned, the menu sits above
        // it, and the scrolled history fills what is left. The caret's row
        // therefore depends only on the pinned rows, never on how the
        // history wraps.
        let panel = self.settings.as_ref().map(|p| p.view(&self.paint, width));
        let panel_h = panel.as_ref().map(|v| v.len()).unwrap_or(0);
        // Both branches leave the bar its row: a menu tall enough to take it
        // would drop whatever that row is saying.
        let room = (self.screen.height as usize).saturating_sub(editor_h + bar_h + 1);
        let menu_h = if panel.is_some() {
            panel_h.min(room)
        } else if menu.is_empty() {
            0
        } else {
            menu.len().min(room)
        };
        // Every pinned row, the bar's included: this is what `Fill(1)` will
        // be left with, and `Rows` fills top-down — a row over that count is
        // dropped off the bottom, where the newest one is.
        let hist_view = (self.screen.height as usize)
            .saturating_sub(editor_h + menu_h + bar_h)
            .max(1);
        let live = self.live(lane, hist_view);

        // While the view is scrolled up, rows the bottom gained since the
        // last measurement fold back into `scroll`, keeping the window put.
        if lane.view.scroll > 0 {
            let items = (lane.view.scrollback.len(), live.len());
            // A frame whose item counts match the last measurement has not
            // grown — nothing to fold, and no reason to re-wrap the history.
            if lane.view.counted.is_none_or(|(sb, lv, _)| (sb, lv) != items) {
                let total = self.scrollback_rows(&lane.view, width) + live.len();
                lane.view.scroll =
                    absorb_growth(lane.view.scroll, lane.view.counted.map(|(_, _, t)| t), total);
                lane.view.counted = Some((items.0, items.1, total));
            }
        } else {
            lane.view.counted = None;
        }
        // Measured in rows, not lines: a line wider than the terminal wraps
        // into several, and counting lines here would put more rows in the
        // area than fit — pushing the newest ones off the bottom, underneath
        // the input, where nothing shows them.
        let scrollback = ScrollbackRows::new(&lane.view.scrollback, &self.paint, &self.done, width);
        let (rows, scroll) = screen::window(
            scrollback.chain(live.iter().map(|s| (Cow::Borrowed(s.as_str()), None))),
            width,
            hist_view,
            lane.view.scroll,
        );

        lane.view.scroll = scroll;
        let items = self.menu_items(&menu);
        let picked = self
            .picked
            .unwrap_or(menu.len().saturating_sub(1))
            .min(menu.len().saturating_sub(1));
        let highlight = self.rat_style(&self.paint.theme.menu.selected);
        let _ = self.screen.draw(|frame| {
            let area = frame.area();
            // The input line is last, so the caret sits on the bottom row and
            // the bar reads as the edge of the history above it rather than
            // as something hanging off the line being typed.
            let chunks = Layout::vertical([
                Constraint::Fill(1),
                Constraint::Length(menu_h as u16),
                Constraint::Length(bar_h as u16),
                Constraint::Length(editor_h as u16),
            ])
            .split(area);
            let (main, menu_area, bar_area, editor_area) =
                (chunks[0], chunks[1], chunks[2], chunks[3]);
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
            if let Some(bar) = &bar {
                frame.render_widget(Rows(std::slice::from_ref(bar)), bar_area);
            }
            frame.render_widget(Rows(&input_view), editor_area);
            let caret_row = editor_area.y + caret_in_view as u16;
            frame.set_cursor_position((caret.1, caret_row));
        });
    }

    /// Rows the scrollback renders to at this width, wraps included.
    fn scrollback_rows(&self, view: &View, width: usize) -> usize {
        ScrollbackRows::new(&view.scrollback, &self.paint, &self.done, width)
            .map(|(text, border)| screen::wrap(border, &text, width).len())
            .sum()
    }

    /// Rebuild the history from the transcript, forgetting everything the old
    /// drawing showed: a rewind changes what the conversation is, and the
    /// screen has to show the new one, not the old one with a note on it.
    fn rebuild(&mut self, view: &mut View, session: &agent::session::Session) {
        view.scrollback.clear();
        // The opening block went with it; a rebuilt screen is the conversation.
        view.opened = 0;
        view.partial.clear();
        view.reasoning = false;
        view.tools.clear();
        view.thinking.streaming = None;
        view.thinking.last = view.thinking.folded;
        view.md.reset();
        view.scroll = 0;
        view.scrollback = scrollback_from(
            session,
            &self.paint,
            &self.bang_prompt,
            &mut view.thinking,
        );
    }

    /// Accept a submitted input: echo it so the prompt survives the editor
    /// being cleared, then fold the block that was current back to the switch
    /// — the input pushes it out of current no matter what it turns out to be.
    fn submit(&mut self, view: &mut View, line: &str) {
        let rows = Row::prompt(line, &self.bang_prompt, &self.paint);
        view.scrollback.extend(rows);
        view.thinking.fold_previous(&mut view.scrollback);
    }

    fn key(&mut self, lane: &mut Lane, event: TermEvent, running: bool) -> Intent {
        let key = match event {
            TermEvent::Resize(w, h) => {
                self.screen.resized(w, h);
                // Re-measuring starts at the new width: the re-wrap is a
                // change of layout, not output, and must not move the view.
                lane.view.counted = None;
                return Intent::None;
            }
            TermEvent::Paste(text) => {
                self.last_esc = None;
                if let Some(v) = &mut self.vim {
                    v.last = None;
                }
                self.editor.insert_str(&text.replace('\r', "\n"));
                return Intent::None;
            }
            TermEvent::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.scroll_view(&mut lane.view, true, 1),
                    MouseEventKind::ScrollDown => self.scroll_view(&mut lane.view, false, 1),
                    _ => {}
                }
                return Intent::None;
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
            _ => return Intent::None,
        };
        let press = Press::of(key.code, key.modifiers);
        // The panel counts as a menu: its own keys are the Menu bindings, and
        // `menu()` is empty while it is open, so the layer has to be forced on.
        let bound = self.keys.action(
            press,
            crate::keys::Layers {
                menu: self.settings.is_some() || !self.menu(running).is_empty(),
                run: running,
                mode: self.vim.as_ref().map(|v| v.mode),
            },
        );

        // A key that means something breaks the escape sequence: `j`, a
        // command, then `k` is two commands and a `j`, not a mode change.
        if bound.is_some() && let Some(v) = &mut self.vim {
            v.last = None;
        }

        // The settings panel owns the menu keys while it is open.
        if let Some(panel) = &mut self.settings {
            match bound {
                Some(Action::MenuNext) => {
                    panel.next();
                    return Intent::None;
                }
                Some(Action::MenuPrevious) => {
                    panel.previous();
                    return Intent::None;
                }
                Some(Action::MenuAccept) => {
                    if panel.editing() {
                        let (path, _) = panel.rows[panel.at].clone();
                        let value = panel.editing_value().to_string();
                        return Intent::CommitSetting(path, value);
                    } else {
                        panel.begin_edit();
                        return Intent::None;
                    }
                }
                Some(Action::MenuDismiss) => {
                    if panel.dismiss() {
                        self.settings = None;
                    }
                    return Intent::None;
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
                        return Intent::None;
                    }
                }
            }
        }

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
                match self.highlighted(running) {
                    Some(MenuEntry::Message { id, .. }) => {
                        self.rewind.clear();
                        return Intent::Rewind(id);
                    }
                    Some(MenuEntry::Completion(c)) => {
                        let line = c.line;
                        // The completion's line is what runs; the typed prefix
                        // that produced it goes, so it cannot be re-submitted
                        // as a stray prompt later.
                        self.editor.take(true);
                        return if line.trim().is_empty() {
                            Intent::None
                        } else {
                            Intent::Submit(line)
                        };
                    }
                    None => {
                        // A secret value must not reach the recall history,
                        // which is written to disk in the clear.
                        let remember = !secret_settings_set(self.editor.text());
                        let typed = self.editor.take(remember);
                        return if typed.trim().is_empty() {
                            Intent::None
                        } else {
                            Intent::Submit(typed)
                        };
                    }
                }
            }
            Some(Action::EditExternally) => return Intent::EditExternally,
            Some(Action::RunInterrupt) => {
                // Esc before the model has moved means "I didn't mean to send
                // that"; an empty editor, or unsending overwrites a line.
                if self.editor.is_empty() && lane.is_running() && !lane.view.committed {
                    return Intent::Unsend;
                }
                return Intent::Interrupt;
            }
            Some(Action::Rewind) => {
                // Double Esc with an empty line opens the rewind selector.
                // The first press only arms it; the second, inside the
                // window, asks the loop for the session's messages.
                if !self.editor.is_empty() {
                    return Intent::None;
                }
                let now = Instant::now();
                if double_tap(&mut self.last_esc, now) {
                    self.last_esc = None;
                    return Intent::OpenRewind;
                }
                return Intent::None;
            }
            Some(Action::LaneNext) => {
                return match self.next_checkout(lane) {
                    Some(name) => Intent::Worktree(name),
                    None => {
                        self.flash("the only checkout there is");
                        Intent::None
                    }
                };
            }
            Some(Action::AppClearScreen) => {
                // One press clears the screen; a second, inside the window,
                // starts a fresh session and rebuilds the screen empty.
                let now = Instant::now();
                if double_tap(&mut self.last_l, now) {
                    self.last_l = None;
                    return Intent::New;
                }
                self.screen.clear();
                return Intent::None;
            }
            Some(Action::AppExit) => {
                // No `running` check: leaving is one intent whatever is in
                // flight, and `admit` gives it one answer.
                return if self.editor.is_empty() {
                    Intent::Quit
                } else {
                    self.editor.delete();
                    Intent::None
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
            Some(Action::MoveWordNext) => self.editor.word_next(),
            Some(Action::ModeInsert) => self.leave_normal(),
            Some(Action::ModeInsertAfter) => {
                self.editor.right();
                self.leave_normal();
            }
            Some(Action::ModeInsertLineStart) => {
                self.editor.home();
                self.leave_normal();
            }
            Some(Action::ModeInsertLineEnd) => {
                self.editor.end();
                self.leave_normal();
            }
            Some(Action::ChangeChar) => {
                self.editor.delete();
                self.leave_normal();
            }
            Some(Action::ChangeToLineEnd) => {
                self.editor.kill_to_end();
                self.leave_normal();
            }
            Some(Action::MoveLineStart) => self.editor.home(),
            Some(Action::MoveLineEnd) => self.editor.end(),
            Some(Action::HistoryOlder) => self.editor.up(),
            Some(Action::HistoryNewer) => self.editor.down(),
            Some(Action::ScrollPageUp) => self.scroll_view(&mut lane.view, true, self.page_scroll_step()),
            Some(Action::ScrollPageDown) => self.scroll_view(&mut lane.view, false, self.page_scroll_step()),
            Some(Action::ScrollHalfUp) => self.scroll_view(&mut lane.view, true, self.half_scroll_step()),
            Some(Action::ScrollHalfDown) => self.scroll_view(&mut lane.view, false, self.half_scroll_step()),
            Some(Action::ThinkFold) => {
                // The last block only: the one streaming, or the newest
                // finished one when nothing is. The switch is left alone, so
                // the blocks no one is touching keep what they had.
                lane.view.thinking.toggle_current(&mut lane.view.scrollback);
            }
            Some(Action::ThinkFoldAll) => {
                // Every block in the scrollback, the last one included, and
                // the switch with them: one key presses the whole screen to a
                // single state.
                lane.view.thinking.flip_all(&mut lane.view.scrollback);
                // A fold-all reflows blocks above the view too; re-baseline.
                lane.view.counted = None;
            }

            Some(Action::MenuAccept) => {
                match self.highlighted(running) {
                    Some(MenuEntry::Message { id, .. }) => {
                        self.rewind.clear();
                        return Intent::Rewind(id);
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
                let n = self.menu(running).len().saturating_sub(1);
                let at = self.picked.unwrap_or(n).min(n);
                self.picked = Some(at.saturating_add(1).min(n));
            }
            Some(Action::MenuPrevious) => {
                let n = self.menu(running).len().saturating_sub(1);
                let at = self.picked.unwrap_or(n).min(n);
                self.picked = Some(at.saturating_sub(1));
            }
            // Answered in the first match, which returns; named here because
            // this one has no catch-all and should not grow one.
            Some(Action::LaneNext) | Some(Action::EditExternally) => {}
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
            // Unbound and printable is the one thing no table has to say —
            // except in Normal, where it is the table saying no.
            None => {
                if let KeyCode::Char(c) = key.code
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    match self.vim.as_mut().map(|v| v.typed(c, Instant::now())) {
                        Some(Typed::Ignore) => {}
                        // The sequence's first half is already on screen: take
                        // it back, so the line says what it means at every
                        // point rather than only once the mode has changed.
                        Some(Typed::Escape) => {
                            self.editor.backspace();
                            self.show_mode();
                        }
                        Some(Typed::Insert) | None => self.editor.insert(c),
                    }
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
        Intent::None
    }

    /// Nudge the scrolled history window by `step` rows, up or down.
    fn scroll_view(&mut self, view: &mut View, up: bool, step: usize) {
        view.scroll = if up {
            view.scroll.saturating_add(step)
        } else {
            view.scroll.saturating_sub(step)
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

    /// Back to Insert. The half-typed escape character goes with the mode: it
    /// belonged to a line nobody is commanding any more.
    fn leave_normal(&mut self) {
        if let Some(v) = &mut self.vim {
            v.mode = Mode::Insert;
            v.last = None;
        }
        self.show_mode();
    }

    /// Put the mode where it can be seen: the gutter's icon and the shape of
    /// the caret. Both, because either alone is missable — the gutter is two
    /// columns away from where the eye is, and the caret is a shape the
    /// terminal may refuse to change.
    ///
    /// This is what pays for the mode never resetting itself. A mode that
    /// persists across submitted lines and cannot be seen would be a trap;
    /// one that can be seen is just where you left it.
    fn show_mode(&mut self) {
        // Three states, not two: vim off is not "Insert", and a caret shaped
        // for a mode nobody turned on is a change to somebody else's terminal.
        let normal = self.vim.as_ref().map(|v| v.mode == Mode::Normal);
        let icon = match normal {
            Some(true) => self.paint.theme.prompt.normal.clone(),
            _ => self.paint.theme.prompt.icon.clone(),
        };
        self.prompt = Self::paint_prompt(&self.paint, &icon);
        self.editor
            .set_prompts(self.prompt.clone(), self.bang_prompt.clone());
        self.screen.cursor_shape(normal);
    }

    /// Follow what the config says about the modal keys.
    ///
    /// Turning them off drops the state rather than parking it: coming back
    /// later in Normal, with no keystroke between having asked to go there,
    /// is the one surprise this has to rule out. Turning them off is also the
    /// only thing that changes the mode without a key — everything else keeps
    /// whichever mode was last asked for, submitted lines included.
    fn set_vim(&mut self, cfg: &crate::config::Vim) {
        match (&mut self.vim, cfg.enabled) {
            (slot @ None, true) => *slot = Some(Vim::new(cfg)),
            (slot, false) => *slot = None,
            (Some(v), true) => v.configure(cfg),
        }
        self.show_mode();
    }

    /// One key, three meanings, and the escalation travels with the binding
    /// rather than with Ctrl-C: stop the run, clear the line, or — pressed
    /// twice inside the window — leave.
    fn interrupt_or_clear(&mut self, running: bool) -> Intent {
        if double_tap(&mut self.last_interrupt, Instant::now()) {
            return Intent::Quit;
        }
        if running {
            return Intent::Interrupt;
        }
        if self.editor.is_empty() {
            self.flash("press it again to quit");
        } else {
            self.editor.clear();
        }
        Intent::None
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
fn land_handled(ui: &mut Ui, core: &Repl, view: &mut View, lines: Vec<String>) {
    view.scrollback.extend(lines.into_iter().map(Row::notice));
    // The key map lives in two places; a reload has to reach both or the
    // screen keeps answering to the old bindings.
    if !Arc::ptr_eq(&ui.keys, &core.keys) {
        ui.keys = core.keys.clone();
    }
    // Likewise the completion list: /reload is allowed to define models — and
    // skills — the last one did not.
    ui.choices = core.choices();
    if ui.paint.theme.as_ref() != &core.config.theme {
        ui.set_theme(view, &core.lane().context, Arc::new(core.config.theme.clone()));
    }
    if !Arc::ptr_eq(&ui.commands, &core.commands) {
        ui.commands = core.commands.clone();
    }
    ui.set_vim(&core.config.vim);
    // The config tree changed under a reload; the `/settings` completion list
    // follows it.
    ui.setting_paths = crate::settings::leaves(&core.file)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
}

// A line submitted while the lane in front is working. What it may do is
// settled before it runs: `command` cannot be asked and then ignored, because
// asking is doing.
//
// Only queueing and refusing happen here. Anything that may run now goes back
// to the loop's own dispatch, so a command takes the same path whether or not
// a run is under way — and a `/worktree` that opens a lane is landed by the
// same code that lands it from an idle prompt.

/// A turn that has ended, on its way back to the loop that started it.
///
/// The transcript comes home this way rather than through a `JoinHandle`, so
/// one channel serves every lane and nothing has to poll a growing list of
/// them. `session` is None only when the run panicked and took its copy down.
/// What woke the loop this time round. One value out of the select rather than
/// a pair of optional locals, so what happened is read in one place.
enum Wake {
    /// Something to carry out, once the select's borrows are gone.
    Do(Intent),
    /// A turn ended and has to be settled, whichever lane it belongs to.
    Turn(Done),
    /// Something only the screen cares about.
    Nothing,
    /// Leave. Not a `break` at the arm: the way out has lanes to settle.
    Leave,
}

/// What kind of job a finished `Done` was, carrying what only that kind
/// leaves behind — so no arm can be built holding another's.
enum Kind {
    /// A turn: the agent loop ran, and `ran` says how it went. Its output
    /// reached the view through the lane's event channel as it happened.
    Turn,
    /// A `!` command, and the lines it printed. They come home whole rather
    /// than as events, so settling is the only place they can be shown.
    ///
    /// Kept apart from a turn's silence because `Bridge` is one accumulator
    /// for the whole surface, filled by whichever turn is streaming and
    /// emptied only by `Event::TurnStart`; a `!` emits no events, so letting
    /// one speak for the bridge flushes another lane's half-written answer to
    /// the phone as though it were finished.
    Bash(Vec<String>),
    /// A `/compact`, and what it shrank and spent. None means the transcript
    /// already fit — or, with a cancelled `ran`, that nobody ever looked.
    Compact(Option<(agent::compact::Report, Totals)>),
}
/// A job that ran off the loop, reporting back to the loop that started it.
///
/// The transcript comes home this way rather than through a `JoinHandle`, so
/// one channel serves every lane and nothing has to poll a growing list of
/// them. `ran` is None only when the job panicked and took its copy down.
struct Done {
    lane: usize,
    kind: Kind,
    /// The transcript back, and how the job went. None when it panicked and
    /// took its copy down with it — one field, because those two are never
    /// separately absent.
    ran: Option<(Session, Result<Totals, AgentError>)>,
}

/// Run `job` off the loop, turning a panic into a `None` the settle side can
/// act on. One guard for every task that carries the transcript, so the
/// panic contract is written once: a job that never reports leaves its lane
/// looking "working" forever.
async fn guard<F, T>(job: F) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    std::panic::AssertUnwindSafe(job).catch_unwind().await.ok()
}

pub struct Tui {
    core: Repl,
    ui: Ui,
    events: UnboundedReceiver<TermEvent>,
    /// Stops the reader while a child holds the terminal.
    hold: Hold,
    totals: Totals,
    bridge: crate::wechat::Bridge,
}

// crossterm reads blockingly, so the keyboard gets a thread of its own and
// reaches the loop as just another channel.
/// What the reader waits on when nothing has been typed. `poll` returns the
/// moment a key arrives, so it costs idle wakeups and no latency.
const INPUT_POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// How long `park` waits to be told the reader stopped. Twice the poll: one
/// just entered has that long before it looks at the flag again.
const PARK_WAIT: std::time::Duration =
    std::time::Duration::from_millis(INPUT_POLL.as_millis() as u64 * 2);

/// How often `park` looks while it waits.
const PARK_STEP: std::time::Duration = std::time::Duration::from_millis(10);

/// The reader's pause switch. Two readers on one stdin would split the user's
/// keystrokes between them, and a thread inside `read` cannot be told to stop.
#[derive(Clone, Default)]
struct Hold {
    paused: Arc<AtomicBool>,
    parked: Arc<AtomicBool>,
}

impl Hold {
    /// Stop reading, and wait to be told it stopped. Bounded: a reader starved
    /// past `PARK_WAIT` is handed the race rather than hanging the editor.
    async fn park(&self) -> Parked {
        self.paused.store(true, Ordering::Release);
        let mut waited = std::time::Duration::ZERO;
        while waited < PARK_WAIT && !self.parked.load(Ordering::Acquire) {
            tokio::time::sleep(PARK_STEP).await;
            waited += PARK_STEP;
        }
        Parked(self.clone())
    }
}

/// SIGINT and SIGQUIT ignored while a child holds the terminal. Cooked mode
/// sends both to the whole foreground group, which this process is in.
#[cfg(unix)]
struct Deafened([libc::sighandler_t; 2]);

#[cfg(unix)]
impl Deafened {
    fn new() -> Self {
        // SAFETY: `signal` is the process-wide disposition; `Drop` puts back
        // exactly what is read here.
        unsafe {
            Self([
                libc::signal(libc::SIGINT, libc::SIG_IGN),
                libc::signal(libc::SIGQUIT, libc::SIG_IGN),
            ])
        }
    }
}

#[cfg(unix)]
impl Drop for Deafened {
    fn drop(&mut self) {
        for (signal, prior) in [(libc::SIGINT, self.0[0]), (libc::SIGQUIT, self.0[1])] {
            // A disposition that could not be read is not one to restore.
            if prior != libc::SIG_ERR {
                // SAFETY: putting back what `new` took, in the same process.
                unsafe { libc::signal(signal, prior) };
            }
        }
    }
}

#[cfg(not(unix))]
struct Deafened;

#[cfg(not(unix))]
impl Deafened {
    fn new() -> Self {
        Self
    }
}

/// Restarts the reader on the way out, however it goes. A reader left parked
/// is a dead keyboard with nothing on screen to say why.
struct Parked(Hold);

impl Drop for Parked {
    fn drop(&mut self) {
        self.0.paused.store(false, Ordering::Release);
    }
}

fn reader() -> (UnboundedReceiver<TermEvent>, Hold) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let hold = Hold::default();
    let mine = hold.clone();
    std::thread::spawn(move || loop {
        if mine.paused.load(Ordering::Acquire) {
            mine.parked.store(true, Ordering::Release);
            std::thread::sleep(INPUT_POLL);
            continue;
        }
        mine.parked.store(false, Ordering::Release);
        match crossterm::event::poll(INPUT_POLL) {
            // Re-checked: the terminal may have gone to a child while this
            // poll was waiting, and that keystroke belongs to the child now.
            Ok(true) if !mine.paused.load(Ordering::Acquire) => {
                match crossterm::event::read() {
                    Ok(event) => {
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
            Ok(_) => {}
            Err(_) => return,
        }
    });
    (rx, hold)
}

/// How long leaving waits for the runs it just cancelled. Long enough for a
/// turn to notice the token, short enough that a wedged one does not hold the
/// terminal hostage.
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

// Where recalled prompts are kept between sessions.
/// `$VISUAL` before `$EDITOR` before `vi`, the order every terminal program
/// that asks uses.
fn external_editor() -> (String, Vec<String>) {
    let raw = ["VISUAL", "EDITOR"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.trim().is_empty()));
    split_editor(raw.as_deref().unwrap_or("vi"))
}

/// Split rather than run whole (`code -w`), and never through a shell, which
/// would make every character in it live. The price is that quoting cannot.
fn split_editor(raw: &str) -> (String, Vec<String>) {
    let mut parts = raw.split_whitespace();
    let program = parts.next().unwrap_or("vi").to_string();
    (program, parts.map(str::to_string).collect())
}

/// A file for the editor to work in, `0600` because `/tmp` is shared and the
/// line holds whatever the user was about to say. `.md` buys highlighting.
fn scratch_file(text: &str) -> std::io::Result<std::path::PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("pi-edit-{}-{stamp}.md", std::process::id()));
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let mut file = open.open(&path)?;
    std::io::Write::write_all(&mut file, text.as_bytes())?;
    Ok(path)
}

fn history_path() -> Option<std::path::PathBuf> {
    tools::state::dir().map(|d| d.join("history"))
}

// Enough to recall from without the file growing without bound.
const HISTORY_KEEP: usize = 1_000;

impl Tui {
    pub fn new(mut core: Repl, keys: Arc<Keys>, bridge: crate::wechat::Bridge) -> Result<Self> {
        let paint = Paint::with_theme(true, Arc::new(core.config.theme.clone()));
        let mut ui = Ui::new(
            Screen::new()?,
            keys,
            core.choices(),
            core.commands.clone(),
            Lists::new(
                core.store.clone(),
                core.lane_mut().ctx.workspace.root().to_path_buf(),
            ),
            paint,
        );
        ui.setting_paths = crate::settings::leaves(&core.file)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        ui.live = core.config.status.live.clone();
        ui.done = core.config.status.done.clone();
        ui.set_vim(&core.config.vim);
        let context = core.lane().context.clone();
        let mut opening = View::opening(&context, &ui.paint);
        opening.model = core.lane().agent.spec.model.clone();
        core.lane_mut().view = opening;
        if let Some(prior) = history_path().and_then(|p| std::fs::read_to_string(p).ok()) {
            ui.editor.seed_history(editor::decode(&prior));
        }
        // A resumed session shows its transcript from the start: the whole
        // screen is rebuildable now, so there is no reason to hide it.
        let lane = core.lane_mut();
        if let Some(session) = lane.session.as_ref().filter(|s| !s.is_empty()) {
            ui.rebuild(&mut lane.view, session);
        }
        let (events, hold) = reader();
        Ok(Self {
            core,
            ui,
            events,
            hold,
            totals: Totals::default(),
            bridge,
        })
    }

    /// A surface on an in-memory screen, for tests that drive the loop's
    /// settle side. No reader thread and no history file: the terminal the
    /// test runner owns is not this test's to touch.
    #[cfg(test)]
    fn on_test_screen(mut core: Repl, keys: Arc<Keys>) -> Self {
        let paint = Paint::with_theme(false, Arc::new(core.config.theme.clone()));
        let ui = Ui::new(
            screen::Screen::test(80, 24),
            keys,
            core.choices(),
            core.commands.clone(),
            Lists::new(
                core.store.clone(),
                core.lane_mut().ctx.workspace.root().to_path_buf(),
            ),
            paint,
        );
        let (_tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            core,
            ui,
            events: rx,
            hold: Hold::default(),
            totals: Totals::default(),
            bridge: crate::wechat::Bridge::new(),
        }
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
    /// Bring the screen into step with the lanes after a command. The view
    /// travels with its lane, so a switch leaves nothing to move: what is left
    /// is the surface's own — the draft, the workspace-keyed lists — and
    /// whatever the lane posted while nobody was looking.
    fn reconcile(&mut self, was: usize) {
        if was == self.core.current {
            return;
        }
        self.ui.leave_lane();
        self.ui.lists.at(self.core.lane().ctx.workspace.root());
        // A lane opened later has no banner yet, and the files it stands on
        // are its own.
        if !self.core.lane().view.drawn {
            let context = self.core.lane().context.clone();
            let opening = View::opening(&context, &self.ui.paint);
            self.core.lane_mut().view = opening;
        }
        // What this lane's run posted while nobody was looking, in the order it
        // arrived. Not through the bridge: the phone follows the lane in front,
        // and replaying an hour of another one into it would be a second
        // conversation arriving out of nowhere.
        for event in std::mem::take(&mut self.core.lane_mut().pending) {
            self.ui.on_event(self.core.lane_mut(), event);
        }
        // And the end of it, if it reached one out of sight.
        if let Some((out, unsend)) = self.core.lane_mut().take_ended() {
            self.close_run(out);
            // Esc asked for the prompt back before the screen moved on. The
            // asking does not go stale because the answer arrived late.
            if unsend
                && let Some(id) = self.core.lane().session.as_ref().and_then(|s| s.last_ask())
            {
                self.rewind_turn(id);
            }
        }
    }

    fn land_swap(&mut self, said: Vec<String>) {
        let lane = self.core.lane_mut();
        if let Some(session) = lane.session.as_ref() {
            self.ui.rebuild(&mut lane.view, session);
        }
        // `at` forgets both lists, so it stands in for `refresh_sessions`: a
        // swap that did not move repeats the root, and drops them either way.
        self.ui.lists.at(self.core.lane_mut().ctx.workspace.root());
        self.core.lane_mut().view.scrollback.extend(said.into_iter().map(Row::notice));
    }

    /// Take what every lane's run has posted since the last look: into the view
    /// when the lane is in front, into its own backlog when it is not.
    ///
    /// A lane out of sight is not drawn — only its transcript has to be kept
    /// whole. What arrived meanwhile is replayed when it comes back.
    async fn serve_lanes(&mut self) {
        for at in 0..self.core.lanes.len() {
            while let Ok(event) = self.core.lanes[at].inbox.try_recv() {
                if at == self.core.current {
                    self.bridge.observe(&event).await;
                    self.ui.on_event(self.core.lane_mut(), event);
                } else {
                    // Deltas arrive thousands at a time and the backlog is
                    // replayed in one go: a run of them folded into one keeps
                    // it the size of what was written rather than of how many
                    // pieces it came in, and the view cannot tell the two apart.
                    let held = &mut self.core.lanes[at].pending;
                    let folded = match (held.last_mut(), &event) {
                        (Some(Event::TextDelta(prev)), Event::TextDelta(next)) => {
                            prev.push_str(next);
                            true
                        }
                        (Some(Event::ReasoningDelta(prev)), Event::ReasoningDelta(next)) => {
                            prev.push_str(next);
                            true
                        }
                        _ => false,
                    };
                    if !folded {
                        held.push(event);
                    }
                }
            }
        }
    }

    /// Rebuild the bottom bar from the lanes, before every draw. A run that
    /// ended out of sight has to reach the screen without anyone asking, and
    /// this is the only thing that looks.
    fn refresh_tabs(&mut self) {
        let current = self.core.current;
        self.ui.tabs = self
            .core
            .lanes
            .iter()
            .enumerate()
            .map(|(at, lane)| Tab {
                // In front is in front, whatever it is doing: the run row above
                // already says whether this one is working.
                mark: if at == current {
                    Mark::Front
                } else {
                    match &lane.turn {
                        Turn::Running { .. } => Mark::Running,
                        Turn::Ended { out: Ok(_), .. } => Mark::Done,
                        Turn::Ended { out: Err(_), .. } => Mark::Failed,
                        Turn::Idle => Mark::Idle,
                    }
                },
                name: lane_name(lane),
            })
            .collect();
    }

    /// Say something about a lane on the screen the user is actually looking
    /// at. A lane that is not in front names itself first, or the notice reads
    /// as news about whichever tree happens to be up.
    /// Carry the lane's loop past a round that has just ended: queue the next
    /// one, or say why there is not one.
    ///
    /// What decides is the tree, never the model: a round that changed a file
    /// is a round whose work was not finished, and one that changed nothing
    /// has nothing left to do. Asking the model instead would hand back the
    /// judgement this exists to take away from it.
    fn step_loop(&mut self, lane: usize, finished: bool) {
        let cap = self.core.config.loop_max_turns;
        let Some(round) = self.core.lanes[lane].loop_step(finished, cap) else {
            return;
        };
        let said = match round {
            crate::lane::Round::Again { goal, next } => {
                self.core.lanes[lane].view.queued.push(Intent::LoopRound(goal));
                format!("loop round {next}")
            }
            crate::lane::Round::Quiet => "loop done — that round changed nothing".to_string(),
            crate::lane::Round::Capped(n) => {
                format!("loop stopped at loop_max_turns ({n}) — rounds were still changing files")
            }
            crate::lane::Round::Cut => "loop stopped".to_string(),
        };
        self.say_of(lane, said);
    }

    /// News from a lane, onto the screen actually being watched rather than
    /// into the lane it came from — where nobody would see it until they
    /// switched. The `whose:` prefix is what makes that readable, and it is
    /// why this lands on `current`: a background lane's own view would need no
    /// name on it.
    fn say_of(&mut self, lane: usize, what: String) {
        let text = if lane == self.core.current {
            what
        } else {
            let whose = self.core.lanes[lane]
                .worktree
                .as_deref()
                .unwrap_or("the main checkout");
            format!("{whose}: {what}")
        };
        self.ui.say(&mut self.core.lane_mut().view, text);
    }

    /// The one gate every input passes: a key, the phone, or an intent coming
    /// back off the queue, so two ways of asking the same thing cannot get two
    /// different answers.
    ///
    /// The answer is in two halves and they live apart on purpose: whether a
    /// run is in flight is state, and is read here; what an intent may do while
    /// one is, is a property of the intent, and `fate` holds it. An idle lane
    /// admits everything, which is why `fate` never has to mention idleness.
    fn admit(&mut self, intent: Intent) -> Wake {
        if !self.core.lane().is_running() {
            return Wake::Do(intent);
        }
        match intent.fate() {
            Fate::Now => Wake::Do(intent),
            Fate::Queued => {
                self.core.lane_mut().view.queued.push(intent);
                Wake::Nothing
            }
            Fate::Refused(why) => {
                self.ui.flash(why);
                Wake::Nothing
            }
        }
    }

    /// Write one edited value from the settings panel, and show the panel what
    /// became of it.
    fn commit_setting(&mut self, path: &str, value: &str) {
        match self.core.commit_file(path, value) {
            Ok(said) => {
                let rows = crate::settings::leaves(&self.core.file);
                self.ui.setting_paths = rows.iter().map(|(p, _)| p.clone()).collect();
                self.core
                    .lane_mut()
                    .view
                    .scrollback
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
    }

    /// Stop the run in front, if there is one. `unsend` also takes the prompt
    /// back once it has stopped.
    fn stop_current(&mut self, unsend: bool) {
        // Said here rather than at the callers: the state is the only thing
        // that knows, and every way of asking to stop arrives through it.
        let Turn::Running { cancel, unsend: take_back } = &mut self.core.lane_mut().turn else {
            self.ui.flash("nothing running to stop");
            return;
        };
        cancel.cancel();
        *take_back = unsend;
        self.core.lane_mut().view.stopping = true;
    }

    /// Drive the terminal until the user leaves.
    ///
    /// No event channel is handed in any more: each lane owns the one its runs
    /// post to, which is what lets a lane the screen has moved on from keep
    /// working without its output landing on somebody else's view.
    pub async fn run(mut self) -> Result<()> {
        // Every lane's runs report here when they end. One channel rather than a
        // handle per lane: the loop waits on it like any other source.
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<Done>();
        let mut tick = tokio::time::interval(crate::status::SPIN);
        // The branch is off while nothing runs, so the interval falls behind
        // the clock; bursting to catch up would spin the loop the moment a run
        // starts. One late tick, then the ordinary cadence.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            self.serve_lanes().await;
            self.refresh_tabs();
            self.ui.flush(self.core.lane_mut());
            let running = self.core.lane().is_running();
            let anywhere = self.core.lanes.iter().any(|lane| lane.is_running());
            // A queued intent waits for the lane it was aimed at to come free.
            let woke = if self.core.lane_mut().view.queued.is_empty() || running {
                // Every branch must be cancel-safe: a loser is dropped mid-poll.
                // `recv()` and `tick()` are; a blocking read gets its own thread.
                tokio::select! {
                    Some(done) = done_rx.recv() => Wake::Turn(done),
                    // Only while something runs, or a flash is up: an idle loop
                    // waking ten times a second is a spinner with nothing to spin.
                    _ = tick.tick(), if anywhere || self.ui.flash.is_some() => {
                        self.ui.spinner += 1;
                        Wake::Nothing
                    }
                    key = self.events.recv() => match key {
                        Some(key) => {
                            let intent = self.ui.key(self.core.lane_mut(), key, running);
                            self.admit(intent)
                        }
                        None => Wake::Leave,
                    },
                    msg = self.bridge.rx.recv() => match msg {
                        // The phone types at the lane in front, like a hand,
                        // and its `/stop` is esc. Same intents, same gate, so
                        // they cannot drift apart.
                        Some(crate::wechat::Inbound::Text { text }) => {
                            self.admit(Intent::Submit(text))
                        }
                        Some(crate::wechat::Inbound::Stop) => self.admit(Intent::Interrupt),
                        Some(crate::wechat::Inbound::Notice(text)) => {
                            self.ui.say(&mut self.core.lane_mut().view, text);
                            Wake::Nothing
                        }
                        None => Wake::Nothing,
                    },
                }
            } else {
                // One at a time, each still the intent it was read as. Joined
                // as lines, a command and a prompt became one line and `read`
                // saw only the first word.
                Wake::Do(self.core.lane_mut().view.queued.remove(0))
            };
            // Out here, where all of `self` is free again.
            let intent = match woke {
                Wake::Turn(done) => {
                    self.settle(done).await;
                    continue;
                }
                Wake::Nothing => continue,
                Wake::Leave => break,
                Wake::Do(intent) => intent,
            };
            // What the surface answers for itself: the screen, the keyboard
            // and the process are not `Repl`'s to move.
            // Whether the line about to run is a loop's own round.
            let mut from_loop = false;
            let intent = match intent {
                Intent::None => continue,
                Intent::Interrupt => {
                    self.stop_current(false);
                    continue;
                }
                Intent::Unsend => {
                    self.stop_current(true);
                    continue;
                }
                Intent::OpenRewind => {
                    self.open_rewind();
                    continue;
                }
                Intent::Rewind(id) => {
                    self.rewind_turn(id);
                    continue;
                }
                Intent::CommitSetting(path, value) => {
                    self.commit_setting(&path, &value);
                    continue;
                }
                Intent::EditExternally => {
                    self.edit_externally().await;
                    continue;
                }
                // A submitted line is echoed and remembered, and only then
                // read — the echo wants the text, which reading spends.
                // A loop's own round: echoed and read like a typed line, and
                // marked so the turn it starts is the one the loop counts.
                Intent::LoopRound(line) => {
                    // The loop that queued this may have been stopped since.
                    // Running it then would be a turn nobody asked for, and
                    // one that reads on screen as if it had been typed.
                    if self.core.lane().looping.is_none() {
                        continue;
                    }
                    from_loop = true;
                    self.ui.submit(&mut self.core.lane_mut().view, &line);
                    self.core.lane_mut().view.scroll = 0;
                    crate::repl::read(&line)
                }
                Intent::Submit(line) => {
                    self.ui.submit(&mut self.core.lane_mut().view, &line);
                    // A fresh turn starts at the newest row: a view scrolled up
                    // to read would otherwise stream output out of sight.
                    self.core.lane_mut().view.scroll = 0;
                    // Written per line rather than on the way out: quitting
                    // with two Ctrl-Cs skips every tidy exit path there is.
                    self.save_history();
                    crate::repl::read(&line)
                }
                // A key that means a command — `ctrl+l` twice is `/new` —
                // arrives already read.
                ready => ready,
            };
            // A loop is the surface's: it arms the lane, then puts its goal
            // back through the door as a typed line — so what runs each round
            // is read exactly as it would be if it had been typed.
            if let Intent::Loop(goal) = intent {
                let goal = goal.trim().to_string();
                if goal.is_empty() {
                    // The round already queued goes with it: run after a stop,
                    // it is a turn nobody asked for and it reads as a typed one.
                    self.core
                        .lane_mut()
                        .view
                        .queued
                        .retain(|q| !matches!(q, Intent::LoopRound(_)));
                    match self.core.lane_mut().looping.take() {
                        // A loop really ended: that belongs in the transcript.
                        Some(l) => {
                            let said =
                                format!("loop stopped after {} round(s) of `{}`", l.round, l.goal);
                            self.ui.say(&mut self.core.lane_mut().view, said);
                        }
                        // Nothing ended — a note about the line, not the lane.
                        None => self.ui.flash(
                            "no loop here — /loop <line> runs one again while \
                             it keeps changing files",
                        ),
                    }
                    continue;
                }
                // Refused rather than replacing: the round already queued would
                // still run, and it would be counted against the new loop.
                if let Some(l) = &self.core.lane().looping {
                    let said = format!("`{}` is already looping here — /loop to stop it first", l.goal);
                    self.ui.flash(said);
                    continue;
                }
                if matches!(crate::repl::read(&goal), Intent::Loop(_)) {
                    self.ui.flash("a loop cannot be its own goal");
                    continue;
                }
                self.core.lane_mut().loop_start(goal.clone());
                self.core.lane_mut().view.queued.push(Intent::LoopRound(goal));
                continue;
            }
            // Bare `/settings` opens the panel instead of going through the
            // line command's read-only list.
            if matches!(intent, Intent::Settings(ref rest) if rest.trim().is_empty()) {
                let rows = crate::settings::leaves(&self.core.file);
                self.ui.settings = Some(settings::Panel::new(rows));
                continue;
            }
            let was = self.core.current;
            let step = self.core.run(intent, &self.totals);
            self.reconcile(was);
            if from_loop {
                // `was`, not whichever lane is in front now: a step may move
                // the surface to another checkout, and the loop belongs to the
                // one that queued the round. Addressed by index, the lane left
                // behind cannot be left armed and unreachable.
                //
                // A round is a turn, and only a step that starts one leaves
                // anything to measure. A line that answers on the spot would
                // leave the loop armed, and the next turn from anywhere would
                // be taken for its round.
                if matches!(step, Step::Prompt { .. } | Step::Bash(_)) {
                    self.core.lanes[was].loop_running();
                } else if let Some(stale) = self.core.lanes[was].looping.take() {
                    let said = format!("loop ended — `{}` starts no turn to measure", stale.goal);
                    self.say_of(was, said);
                }
            }
            match step {
                Step::Quit => break,
                Step::Flash(line) => self.ui.flash(line),
                Step::Bash(command) => self.start_bash(command, &done_tx),
                Step::Swap(said) => self.land_swap(said),
                Step::Handled(lines) => {
                    // Lent out rather than borrowed in place: the view lives
                    // inside `core`, which this also reads.
                    let at = self.core.current;
                    let mut view = std::mem::take(&mut self.core.lanes[at].view);
                    land_handled(&mut self.ui, &self.core, &mut view, lines);
                    self.core.lanes[at].view = view;
                }
                Step::Compact(focus) => self.start_compact(focus, &done_tx),
                Step::Wechat(cmd) => {
                    let said = match cmd {
                        repl::WechatCmd::Status => self.bridge.status(),
                        // Only local locks and a client build await here; the
                        // login and long poll already run in their own tasks.
                        repl::WechatCmd::On => match self.bridge.on().await {
                            Ok(said) => said,
                            Err(e) => {
                                self.ui.say(&mut self.core.lane_mut().view, format!("wechat: {e:#}"));
                                Vec::new()
                            }
                        },
                        repl::WechatCmd::Off => self.bridge.off(),
                    };
                    self.core.lane_mut().view.scrollback
                        .extend(said.into_iter().map(Row::notice));
                }
                // What was submitted while the run worked is taken up by the
                // top of this loop, one entry at a time and each read as what
                // it is. Draining it here instead meant everything queued
                // became the next prompt, whatever it had been typed as.
                Step::Prompt { send, typed } => self.start_turn(send, typed, &done_tx),
            }
        }
        self.save_history();
        self.settle_all(&mut done_rx).await;
        Ok(())
    }

    /// Stop every lane still working and settle it, so leaving cannot drop a
    /// transcript that lives in a task.
    ///
    /// Cancelling, not waiting for the work to finish: a run stops at its next
    /// cancellation point, which is the wait Esc already asks of anyone. The
    /// deadline is for the run that will not stop — what is on disk is then the
    /// last save, which is what leaving without this gave every time.
    async fn settle_all(&mut self, done: &mut UnboundedReceiver<Done>) {
        let mut left = 0;
        for lane in &self.core.lanes {
            if let Turn::Running { cancel, .. } = &lane.turn {
                cancel.cancel();
                left += 1;
            }
        }
        if left == 0 {
            return;
        }
        self.ui.say(&mut self.core.lane_mut().view, "stopping — saving what the runs have written");
        self.ui.flush(self.core.lane_mut());
        let waited = tokio::time::timeout(EXIT_GRACE, async {
            while left > 0 {
                match done.recv().await {
                    Some(ended) => {
                        self.settle(ended).await;
                        left -= 1;
                    }
                    None => break,
                }
            }
        })
        .await;
        if waited.is_err() {
            // On the terminal we are about to give back: a transcript that did
            // not come home is one the user should know is short.
            self.ui.screen.leave();
            eprintln!("{left} run(s) did not stop in time; their last save is what is on disk");
        }
    }

    /// Cut the transcript at an entry — chosen from the selector, or the
    /// prompt an Esc took back — and say what is left.
    fn rewind_turn(&mut self, id: EntryId) {
        match self.core.rewind_to(id) {
            Ok(Rewound::Nothing) => {
                self.ui.flash(NOTHING_TO_REWIND);
            }
            Ok(outcome) => {
                // The transcript is the source of truth again: rebuild the
                // whole view from it, so the screen returns to the node the
                // conversation did instead of keeping the forgotten turns.
                // It clears anything said before it: hence the notice after.
                let lane = self.core.lane_mut();
                if let Some(session) = lane.session.as_ref() {
                    self.ui.rebuild(&mut lane.view, session);
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
                            .lane_mut()
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
                let text = self.ui.paint.on(&self.ui.paint.theme.muted, &said);
                self.ui.say(
                    &mut self.core.lane_mut().view, text);
            }
            Err(e) => {
                self.ui.say(
                    &mut self.core.lane_mut().view,
                    format!("warning: the transcript was not saved: {e}"),
                );
            }
        }
    }

    /// Open the rewind selector on every point the conversation can go back
    /// to: what the user asked, and what the model answered.
    fn open_rewind(&mut self) {
        let rows: Vec<MenuEntry> = self
            .core
            .lane_mut()
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
            self.ui.flash(NOTHING_TO_REWIND);
            return;
        }
        self.ui.open_rewind(rows);
    }

    /// Start a turn on the lane in front and come straight back.
    ///
    /// The run keeps the transcript for its length and posts what it is doing
    /// to that lane's own channel, so the loop is free to draw, read keys and
    /// serve the other lanes — including this one after the screen moves on.
    /// Hand the view over to a job about to start: the clock runs, and the
    /// per-run figures start from nothing rather than from the last run's.
    /// `committed` says whether the prompt behind it can still be taken back.
    fn arm_view(&mut self, committed: bool) {
        self.core.lane_mut().view.started = Some(std::time::Instant::now());
        self.core.lane_mut().view.committed = committed;
        self.core.lane_mut().view.stopping = false;
        self.core.lane_mut().view.tally.clear();
    }

    fn start_turn(&mut self, prompt: String, typed: Option<String>, done: &UnboundedSender<Done>) {
        // Lent to the run for the length of the turn. A lane with a run under
        // way refuses another, so the only way it is missing here is the lane
        // whose transcript a panic took and whose archive would not read back.
        let Some(mut carried) = self.core.lane_mut().session.take() else {
            self.ui.flash(NO_TRANSCRIPT);
            return;
        };
        carried.send_prompt(prompt, typed);
        let cancel = CancellationToken::new();
        let ctx = self.core.lane_mut().ctx.clone().with_cancel(cancel.clone());

        self.arm_view(false);
        // Read while the agent is still reachable: `/model` may replace it
        // while this run works, and the run keeps the one it started on.
        self.core.lane_mut().view.model = self.core.lane_mut().agent.spec.model.clone();

        let agent = self.core.lane_mut().agent.clone();
        let sent = self.core.lane_mut().events.clone();
        let lane = self.core.current;
        let done = done.clone();
        tokio::spawn(async move {
            let out = guard(agent.run(&mut carried, &ctx, &sent)).await;
            let _ = done.send(Done {
                lane,
                kind: Kind::Turn,
                ran: out.map(|out| (carried, out)),
            });
        });
        self.core.lane_mut().turn = Turn::Running {
            cancel,
            unsend: false,
        };
    }

    /// Run a `!` command the way a turn runs: off the loop, so the screen stays
    /// live and the lane can be left to it.
    ///
    /// It borrows the transcript like a turn, and for the same reason: the
    /// result is filed in it, and nothing else may replace it meanwhile.
    /// Hand the terminal to `$EDITOR` on a copy of the line, and take back what
    /// was saved. On its own thread: a run in flight still has a stream to serve.
    async fn edit_externally(&mut self) {
        let path = match scratch_file(self.ui.editor.text()) {
            Ok(path) => path,
            Err(e) => {
                self.ui.flash(format!("no scratch file: {e}"));
                return;
            }
        };
        let (program, args) = external_editor();

        // The terminal is gone from here to `resume`, so nothing between them
        // may return early: the surface would be left invisible.
        let _parked = self.hold.park().await;
        let _deaf = Deafened::new();
        self.ui.screen.leave();
        let ran = tokio::task::spawn_blocking({
            let path = path.clone();
            move || {
                let mut cmd = std::process::Command::new(program);
                cmd.args(args).arg(&path);
                // SIG_IGN is inherited across exec, and an editor installing
                // no handler would be the one that could not be interrupted.
                #[cfg(unix)]
                unsafe {
                    use std::os::unix::process::CommandExt;
                    cmd.pre_exec(|| {
                        libc::signal(libc::SIGINT, libc::SIG_DFL);
                        libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                        Ok(())
                    });
                }
                cmd.status()
            }
        })
        .await;
        let resumed = self.ui.screen.resume();
        self.ui.show_mode();
        // A resize while the child held the terminal raised no event, so the
        // view's measurements are against a width that may no longer exist.
        self.core.lane_mut().view.counted = None;

        // Judged on its own: a save that succeeded is still a save when the
        // screen comes back badly, and reading the two together threw it away.
        let (mut keep, mut said) = match ran {
            Err(e) => (false, Some(format!("the editor did not run: {e}"))),
            Ok(Err(e)) => (false, Some(format!("could not run the editor: {e}"))),
            // `:cq` is how vim says "forget it". Git reads a non-zero exit the
            // same way, and the line the user had is worth more than the file.
            Ok(Ok(s)) if !s.success() => {
                (false, Some(format!("editor exited {s} — the line is unchanged")))
            }
            Ok(Ok(_)) => match std::fs::read_to_string(&path) {
                Ok(text) => {
                    self.ui.editor.set_line(text.trim_end());
                    (false, None)
                }
                // Kept: what was written is the only copy of it, and naming
                // the file beats deleting it.
                Err(e) => (
                    true,
                    Some(format!("saved at {} — could not read it back: {e}", path.display())),
                ),
            },
        };
        // A surface that did not come back cannot show anything, so the file
        // stays as the way out and its name is what the message carries.
        if let Err(e) = resumed {
            keep = true;
            said = Some(format!("the screen did not come back: {e} — line at {}", path.display()));
        }
        if !keep {
            let _ = std::fs::remove_file(&path);
        }
        if let Some(line) = said {
            self.ui.flash(line);
        }
    }

    fn start_bash(&mut self, command: String, done: &UnboundedSender<Done>) {
        let Some(mut carried) = self.core.lane_mut().session.take() else {
            self.ui.flash(NO_TRANSCRIPT);
            return;
        };
        let cancel = CancellationToken::new();
        let ctx = self.core.lane_mut().ctx.clone().with_cancel(cancel.clone());
        // Committed forecloses `Intent::Unsend`, the only writer of
        // `Turn::Running.unsend`, which a `!` has no prompt to honour.
        self.arm_view(true);

        let lane = self.core.current;
        let done = done.clone();
        tokio::spawn(async move {
            let out = guard(async move {
                let out = crate::repl::run_bash(&ctx, &command).await;
                // Esc that stopped the `!` is a cancelled run too; `ran` says
                // so instead of a success that spent nothing.
                let ran = if ctx.cancel.is_cancelled() {
                    Err(AgentError::Cancelled)
                } else {
                    Ok(Totals::default())
                };
                crate::repl::record_bash(&mut carried, &command, out.text);
                (carried, ran, out.said)
            })
            .await;
            // A panic printed nothing anyone can still show; the empty lines
            // and the missing transcript say the same thing from both sides.
            let (kind, ran) = match out {
                Some((carried, ran, said)) => (Kind::Bash(said), Some((carried, ran))),
                None => (Kind::Bash(Vec::new()), None),
            };
            let _ = done.send(Done { lane, kind, ran });
        });
        self.core.lane_mut().turn = Turn::Running {
            cancel,
            unsend: false,
        };
    }

    /// Run a `/compact` off the loop. It can spend real time — summarising
    /// what it drops is a model call — and the lane must keep drawing and
    /// serving the others meanwhile.
    fn start_compact(&mut self, focus: Option<String>, done: &UnboundedSender<Done>) {
        let Some(mut carried) = self.core.lane_mut().session.take() else {
            self.ui.flash(NO_TRANSCRIPT);
            return;
        };
        let cancel = CancellationToken::new();
        // Work under way like any turn's, and nothing anyone can take back.
        self.arm_view(true);

        let agent = self.core.lane_mut().agent.clone();
        let lane = self.core.current;
        let done = done.clone();
        let stop = cancel.clone();
        tokio::spawn(async move {
            let out = guard(async move {
                // Dropped mid-flight, not signalled: `compact_now` writes to
                // the transcript only once every await is behind it.
                let ran = tokio::select! {
                    got = agent.compact_now(&mut carried, focus.as_deref()) => Ok(got),
                    _ = stop.cancelled() => Err(AgentError::Cancelled),
                };
                (carried, ran)
            })
            .await;
            // A report only exists where the pass ran to the end: `Ok(None)`
            // is a transcript that already fit, `Err` one nobody looked at.
            let (kind, ran) = match out {
                Some((carried, Ok(got))) => {
                    (Kind::Compact(got), Some((carried, Ok(Totals::default()))))
                }
                Some((carried, Err(e))) => (Kind::Compact(None), Some((carried, Err(e)))),
                None => (Kind::Compact(None), None),
            };
            let _ = done.send(Done { lane, kind, ran });
        });
        self.core.lane_mut().turn = Turn::Running {
            cancel,
            unsend: false,
        };
    }

    /// A job came home. Every kind settles on the lane that lent it the
    /// transcript, whichever lane is on screen by the time it lands.
    async fn settle(&mut self, done: Done) {
        // The run posts its last events and only then says it is over, so both
        // are in flight at once and the end can win the race. Take what is
        // waiting before closing anything, or a tool row still open is frozen
        // as abandoned and the elapsed figure is read off a cleared clock.
        self.serve_lanes().await;
        // Here rather than in the arms below, so a kind added later cannot
        // forget it and leave the lane queueing prompts it will never run.
        let unsend = self
            .core
            .lanes
            .get_mut(done.lane)
            .is_some_and(Lane::finish);
        match done.kind {
            Kind::Turn | Kind::Bash(_) => self.settle_run(done, unsend).await,
            Kind::Compact(_) => self.settle_compact(done).await,
        }
    }

    /// A turn or a `!` has ended. Put the transcript back and save it,
    /// whichever lane it belongs to; show the end of it only when that lane
    /// is the one on screen.
    ///
    /// The saving cannot wait — a lane the user never returns to still has to
    /// have its work on disk — but nothing about drawing it does.
    async fn settle_run(&mut self, done: Done, unsend: bool) {
        let Done { lane, ran, kind } = done;
        // Only a turn is a request the model was working on, and only a turn's
        // ending is worth telling it about.
        let was_turn = matches!(kind, Kind::Turn);
        // A `!` brings its lines home to be shown here; a turn's reached the
        // view as events, and a compact never arrives at this function.
        let said = match kind {
            Kind::Bash(lines) => Some(lines),
            _ => None,
        };

        // Only a run that came back says why it ended; the archive rebuild
        // below has the same transcript and knows nothing about the run.
        let ran_back = ran.is_some();

        // The task carried the whole transcript, not just this turn, and a
        // panic in it dropped that copy. The archive is the last good one;
        // carrying the empty stand-in forward would save it over the real one
        // at the end of the next turn, which loses the conversation rather
        // than the turn.
        let (recovered, out) = match ran {
            Some((session, out)) => {
                self.core.lanes[lane].session = Some(session);
                (true, out)
            }
            None => (self.recover_session(lane, "run"), Err(AgentError::Cancelled)),
        };

        // A panic never came back, and Esc that took the prompt back produced
        // nothing to misread as a task: neither has anything to tell. Nor does
        // a `!` the user stopped — the shell command was theirs, and calling
        // it a cancelled run tells the model to abandon a request it never had.
        if ran_back
            && !unsend
            && was_turn
            && let Some(session) = self.core.lanes[lane].session.as_mut()
        {
            session.note_outcome(&out);
        }

        // Saved either way: an interrupted turn is exactly the one worth
        // keeping. Not when the transcript never came back, though — the empty
        // one standing in for it would land on top of what is on disk.
        if recovered
            && let Err(e) = self.core.save_lane(lane)
        {
            self.say_of(lane, format!("warning: the transcript was not saved: {e}"));
        }
        // The save put the session on disk, the one `/resume` is likeliest to
        // want back; make the completion list see it.
        self.refresh_sessions();

        let cancelled = matches!(&out, Err(AgentError::Cancelled));
        if said.is_none() && lane == self.core.current {
            self.bridge.finish_turn(cancelled).await;
        }
        // The run's totals (its subagents' included) land on the lane it ran
        // in, and on the surface's grand total, so `/cost` can split the bill.

        if let Ok(totals) = &out {
            self.core.lanes[lane].totals.merge(totals);
            self.totals.merge(totals);
        }

        // A `!` command's output comes home whole rather than as events, so
        // this is the only place it can reach the view that asked for it.
        if let Some(said) = said.filter(|lines| !lines.is_empty()) {
            let rows = said.into_iter().map(Row::notice);
            self.core.lanes[lane].view.scrollback.extend(rows);
        }

        // Before the split below, so a round that ended off-screen still arms
        // the next one — it waits with the lane, like any queued line.
        let finished = out.is_ok() && !unsend;
        self.step_loop(lane, finished);

        // Out of sight: what the run left to draw waits with it, and the lane
        // says so in the bar until someone looks.
        if lane != self.core.current {
            self.core.lanes[lane].turn = Turn::Ended { out, unsend };
            return;
        }
        self.close_run(out);
        if unsend
            && let Some(id) = self.core.lane().session.as_ref().and_then(|s| s.last_ask())
        {
            self.rewind_turn(id);
        }
    }

    /// Put back the archive when the job that borrowed the live transcript
    /// never returned it, naming the job as `verb`. Says whether the lane now
    /// holds something safe to write over its save — without one it refuses
    /// work until `/new`, where exiting would cost every other lane its run.
    fn recover_session(&mut self, lane: usize, verb: &str) -> bool {
        let id = self.core.lanes[lane].id.clone();
        match self.core.store.load(&id) {
            Ok(stored) => {
                self.core.lanes[lane].session = Some(stored.into_session());
                self.say_of(
                    lane,
                    format!("the {verb} did not finish — back to the transcript as last saved"),
                );
                true
            }
            Err(why) => {
                self.say_of(
                    lane,
                    format!(
                        "the {verb} did not finish and its transcript could not be read back \
                         ({why}) — /new or /resume to use this checkout again"
                    ),
                );
                false
            }
        }
    }

    /// A `/compact` has finished: put the transcript back, and show what the
    /// pass did — or say there was nothing to shrink.
    async fn settle_compact(&mut self, done: Done) {
        let Done { lane, ran, kind } = done;
        let Kind::Compact(report) = kind else {
            unreachable!("only a compact settles here")
        };
        let stopped = matches!(&ran, Some((_, Err(AgentError::Cancelled))));
        // The lane it was started on may not be the one on screen any more;
        // give it back its transcript either way.
        let back = match ran {
            Some((session, _)) => {
                self.core.lanes[lane].session = Some(session);
                true
            }
            None => self.recover_session(lane, "compaction"),
        };

        if let Some((report, spent)) = report {
            self.core.lanes[lane].totals.merge(&spent);
            self.totals.merge(&spent);
            let _ = self.core.lanes[lane].events.send(Event::Compacted(report));
            if back
                && let Err(e) = self.core.save_lane(lane)
            {
                self.say_of(lane, format!("warning: the transcript was not saved: {e}"));
            }
            self.refresh_sessions();
        } else if stopped {
            // Nothing was written, so there is nothing to report and nothing
            // to save — only the same word a stopped turn ends on.
            self.say_of(lane, self.ui.paint.on(&self.ui.paint.theme.muted, "stopped"));
        } else if back {
            let held = self.core.lanes[lane].agent.kept_tokens().unwrap_or(0);
            let now = self.core.tokens_now_at(lane);
            self.say_of(
                lane,
                format!(
                    "nothing to compact — {now} tokens, all inside the {held} kept as working context"
                ),
            );
        }
        // The pass is over, so the clock stops. What it was driving — the
        // live region — already went with the turn.
        self.core.lanes[lane].view.started = None;
    }

    /// Draw the end of a run into the view that is on screen.
    fn close_run(&mut self, out: Result<Totals, AgentError>) {
        self.ui.close(&mut self.core.lane_mut().view);
        // A cancelled run's calls got no `ToolEnd`; their animated rows have to
        // reach scrollback some other way before the next flush draws them as a
        // frozen spinner.
        self.ui.abandon_tools(&mut self.core.lane_mut().view);
        self.core.lane_mut().view.started = None;
        match out {
            Ok(_) => {}
            Err(AgentError::Cancelled) => {
                let text = self.ui.paint.on(&self.ui.paint.theme.muted, "stopped");
                self.ui.say(
                    &mut self.core.lane_mut().view, text);
            }
            Err(e) => {
                let text = format!(
                    "{} {e}",
                    self.ui.paint.on(&self.ui.paint.theme.status.err, "error")
                );
                self.ui.say(&mut self.core.lane_mut().view, text);
            }
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
        assert!(!secret_settings_set("/cost"));
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
        let rows = scrollback_from(&s, &Paint::new(false), "! ", &mut thinking);
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
        let shown: Vec<String> = ScrollbackRows::new(&rows, &paint, &[], 80)
            .map(|(r, _)| r.to_string())
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
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, &[], 80)
            .map(|(s, _)| s)
            .collect();
        assert_eq!(rows, vec!["thinking · 2 lines"]);
    }

    #[test]
    fn an_unfolded_entry_shows_its_lines() {
        let rows = [block(1, 2, false)];
        let paint = Paint::new(false);
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, &[], 80)
            .map(|(s, _)| s)
            .collect();
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
        let a: Vec<Cow<'_, str>> = ScrollbackRows::new(&live, &paint, &[], 80)
            .map(|(s, _)| s)
            .collect();
        let b: Vec<Cow<'_, str>> = ScrollbackRows::new(&rebuilt, &paint, &[], 80)
            .map(|(s, _)| s)
            .collect();
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

        let narrow: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, &[], 40)
            .map(|(s, _)| s)
            .collect();
        let wide: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, &[], 160)
            .map(|(s, _)| s)
            .collect();
        // And back again: widening must not be the only direction that repaints.
        let again: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, &[], 40)
            .map(|(s, _)| s)
            .collect();

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
        let out: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, &[], 80)
            .map(|(s, _)| s)
            .collect();
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
        let narrow: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, &[], 40)
            .map(|(s, _)| s)
            .collect();
        let wide: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, &[], 160)
            .map(|(s, _)| s)
            .collect();

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
        let bad: Vec<Cow<'_, str>> = ScrollbackRows::new(&bad, &paint, &[], 80)
            .map(|(s, _)| s)
            .collect();
        let good: Vec<Cow<'_, str>> = ScrollbackRows::new(&good, &paint, &[], 80)
            .map(|(s, _)| s)
            .collect();
        assert!(bad[0].starts_with('✗'), "{}", bad[0]);
        assert!(good[0].starts_with('✓'), "{}", good[0]);
    }

    #[test]
    fn a_plain_entry_is_itself() {
        let rows = [Row::notice("hello".to_string())];
        let paint = Paint::new(false);
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(&rows, &paint, &[], 80)
            .map(|(s, _)| s)
            .collect();
        assert_eq!(rows, vec!["hello"]);
    }

    #[test]
    fn an_empty_scrollback_iterates_to_nothing() {
        // Both walks index `rows[0]` before comparing their pointers, so an
        // empty scrollback panicked. The back walk kept doing it after the
        // front was fixed, and `screen::window` is the one that walks back.
        let paint = Paint::new(false);
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(&[], &paint, &[], 80)
            .map(|(s, _)| s)
            .collect();
        assert!(rows.is_empty());
        let back: Vec<Cow<'_, str>> = ScrollbackRows::new(&[], &paint, &[], 80)
            .rev()
            .map(|(s, _)| s)
            .collect();
        assert!(back.is_empty(), "the back walk too");
    }

    #[test]
    fn scrollback_rows_walk_from_both_ends() {
        let rows = vec![
            Row::notice("a".to_string()),
            block(1, 2, false),
            Row::notice("d".to_string()),
        ];
        let paint = Paint::new(false);
        let rows = ScrollbackRows::new(&rows, &paint, &[], 80);
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
        let front: Vec<&str> = front.iter().map(|(s, _)| s.as_ref()).collect();
        let back: Vec<&str> = back.iter().map(|(s, _)| s.as_ref()).collect();
        assert_eq!(front, vec!["a", "line 1"]);
        assert_eq!(back, vec!["d", "line 2"]);
    }

    /// A said line wider than the terminal keeps its rule down every row it
    /// wraps to. Before the border lived apart from the text, the wrap cut it
    /// at the first row and the rest of the line ran flush against the left
    /// edge — the bar ended mid-air.
    #[test]
    fn a_wrapped_said_line_keeps_its_rule_down_every_row() {
        let paint = Paint::new(false);
        let rows = Row::prompt(&"curl ".repeat(30), "! ", &paint);
        let scrollback = ScrollbackRows::new(&rows, &paint, &[], 20);
        let (out, _) = screen::window(scrollback, 20, 10, 0);
        assert!(out.len() > 1, "a line wider than the window wraps");
        for (i, row) in out.iter().enumerate() {
            assert!(
                row.starts_with("\u{258c} "),
                "row {i} lost the rule: {row:?}"
            );
        }
        // Nothing is lost either: the wrapped rows still spell the whole line.
        let joined: String = out
            .iter()
            .map(|row| {
                crate::render::strip_ansi(row)
                    .trim_start_matches("\u{258c} ")
                    .to_string()
            })
            .collect();
        assert_eq!(joined, "curl ".repeat(30));
    }

    /// A `!` command keeps its own mark and nothing is repeated: its wrapped
    /// rows are the command's continuation, not a new command each row.
    #[test]
    fn a_wrapped_bang_command_repeats_nothing() {
        let paint = Paint::new(false);
        let rows = Row::prompt(&format!("!{}", "go ".repeat(30)), "! ", &paint);
        let scrollback = ScrollbackRows::new(&rows, &paint, &[], 20);
        let (out, _) = screen::window(scrollback, 20, 10, 0);
        assert!(out.len() > 1);
        assert!(out[0].starts_with("! "));
        for row in &out[1..] {
            assert!(!row.starts_with("! "), "a continuation row wore the mark: {row:?}");
        }
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
        let rows: Vec<Cow<'_, str>> = ScrollbackRows::new(std::slice::from_ref(&entry), &paint, &[], 80)
            .map(|(s, _)| s)
            .collect();
        assert_eq!(rows, vec!["line 1", "line 2"]);
        entry.set_folded(true);
        let rows: Vec<Cow<'_, str>> =
            ScrollbackRows::new(std::slice::from_ref(&entry), &paint, &[], 80)
                .map(|(s, _)| s)
                .collect();
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
            content.iter().map(|s| (Cow::Borrowed(s.as_str()), None)),
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

    // ---------------------------------------------------------- settling

    /// A lane wired up enough to be settled: a real transcript, a real agent,
    /// and a `Turn::Running` standing in for the job that is about to report.
    /// A lane and the directory it lives in — the guard comes back so the
    /// caller keeps it alive for as long as the lane is used.
    fn a_running_lane() -> (tempfile::TempDir, crate::lane::Lane) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let lane = running_lane(dir.path());
        (dir, lane)
    }

    fn running_lane(dir: &std::path::Path) -> crate::lane::Lane {
        struct Mute;
        #[async_trait::async_trait]
        impl brain::Transport for Mute {
            async fn stream(
                &self,
                _spec: &brain::model::ModelSpec,
                _req: &brain::request::Request,
            ) -> brain::Result<
                futures::stream::BoxStream<'static, brain::Result<brain::stream::StreamEvent>>,
            > {
                Ok(Box::pin(futures::stream::empty()))
            }
        }
        let ws = tools::Workspace::new(dir).expect("a workspace");
        let spec = brain::model::ModelSpec {
            model: "m".into(),
            base_url: "http://localhost".into(),
            format: brain::model::Format::Anthropic {
                cache_control: brain::model::CacheControl::Off,
            },
            context_window: 200_000,
            max_output_tokens: 8_000,
            vision: false,
            thinking: None,
            accepts_temperature: true,
            can_force_tool: true,
            replay_thinking: brain::model::ReplayThinking::Tagged,
            pricing: brain::model::Pricing::default(),
        };
        let (events, inbox) = crate::lane::Lane::channel();
        crate::lane::Lane {
            agent: std::sync::Arc::new(agent::Agent::new(std::sync::Arc::new(Mute), spec)),
            session: None,
            id: "s1".into(),
            created: 0,
            name: None,
            totals: agent::Totals::default(),
            context: Vec::new(),
            standing: std::sync::Arc::from(""),
            ctx: tools::Ctx::new(ws),
            worktree: None,
            events,
            inbox,
            pending: Vec::new(),
            looping: None,
            // What every `start_*` leaves behind while its job runs.
            turn: crate::lane::Turn::Running {
                cancel: tokio_util::sync::CancellationToken::new(),
                unsend: false,
            },
            view: Default::default(),
            keys: std::sync::Arc::new(crate::keys::Keys::default()),
            commands: std::sync::Arc::new(Vec::new()),
        }
    }

    fn surface(dir: &std::path::Path) -> super::Tui {
        let keys = std::sync::Arc::new(crate::keys::Keys::default());
        let core = crate::repl::Repl {
            store: crate::session::Store::new(dir.join("state")),
            keys: keys.clone(),
            config: std::sync::Arc::new(crate::config::Config::default()),
            args: std::sync::Arc::new(<crate::Args as clap::Parser>::parse_from(["pi"])),
            commands: std::sync::Arc::new(Vec::new()),
            file: toml::Value::Table(Default::default()),
            claimed: Default::default(),
            lanes: vec![running_lane(dir)],
            current: 0,
        };
        super::Tui::on_test_screen(core, keys)
    }

    /// A flash belongs to the lane it answered. Carried across a switch it
    /// names the wrong checkout, and it does it on the row the lane strip
    /// would have used to say which checkout you just landed in.
    #[tokio::test]
    async fn a_flash_does_not_follow_the_surface_to_another_lane() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut tui = surface(dir.path());
        tui.core.lanes.push(running_lane(dir.path()));
        tui.ui.tabs = vec![
            super::Tab { mark: super::Mark::Idle, name: "pi-rs".into() },
            super::Tab { mark: super::Mark::Front, name: "f1".into() },
        ];

        tui.ui.flash("nothing running to stop");
        tui.core.current = 1;
        tui.reconcile(0);
        assert!(tui.ui.flash.is_none(), "the flash was left behind");

        tui.ui.flush(&mut tui.core.lanes[1]);
        let painted = tui.ui.screen.painted();
        assert_eq!(
            painted[painted.len() - 2].trim(),
            "pi-rs \u{b7} f1",
            "the strip has its row back: {painted:?}"
        );
    }

    /// A rebuilt lane has already been drawn, whatever its row counts say.
    /// `rebuild` clears the banner along with the rest — `/resume` and a
    /// rewind both do it — so a switch back must not read that as a lane
    /// never drawn and lay a fresh opening block over the transcript.
    #[tokio::test]
    async fn switching_back_to_a_rebuilt_lane_keeps_its_transcript() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut tui = surface(dir.path());
        tui.core.lanes.push(running_lane(dir.path()));

        // Lane 1 as `rebuild` leaves it: the conversation, and no banner.
        tui.core.lanes[1].view = super::View::opening(&[], &tui.ui.paint);
        tui.core.lanes[1].view.scrollback = vec![Row::notice("what was said before")];
        tui.core.lanes[1].view.opened = 0;

        tui.core.current = 1;
        tui.reconcile(0);

        // By content, not by count: the banner this would lay over it is one
        // row too, so a length check cannot tell them apart.
        let paint = Paint::new(false);
        let rows: Vec<String> = ScrollbackRows::new(&tui.core.lanes[1].view.scrollback, &paint, &[], 80)
            .map(|(r, _)| r.to_string())
            .collect();
        assert!(
            rows.iter().any(|r| r.contains("what was said before")),
            "the transcript survives the switch: {rows:?}"
        );
    }

    /// The bug this guards: `/compact` used to settle without ever putting the
    /// lane back to `Idle`, and a lane left `Running` queues every later
    /// prompt into a queue that only drains once it is not running — so the
    /// checkout was wedged for good. Every kind has to come back idle.
    #[tokio::test]
    async fn every_kind_of_job_leaves_its_lane_idle() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let kinds = || {
            vec![
                ("turn", super::Kind::Turn),
                ("bash", super::Kind::Bash(vec!["out".into()])),
                ("compact", super::Kind::Compact(None)),
                (
                    "compact with a report",
                    super::Kind::Compact(Some((
                        agent::compact::Report::default(),
                        agent::Totals::default(),
                    ))),
                ),
            ]
        };
        for (what, kind) in kinds() {
            let mut tui = surface(dir.path());
            tui.settle(super::Done {
                lane: 0,
                kind,
                ran: Some((
                    agent::session::Session::default(),
                    Ok(agent::Totals::default()),
                )),
            })
            .await;
            assert!(
                !tui.core.lanes[0].is_running(),
                "a {what} left its lane running"
            );
        }
        // And the same when the job panicked and brought no transcript home.
        for (what, kind) in kinds() {
            let mut tui = surface(dir.path());
            tui.settle(super::Done { lane: 0, kind, ran: None }).await;
            assert!(
                !tui.core.lanes[0].is_running(),
                "a panicked {what} left its lane running"
            );
        }
    }

    /// A `!` the user stopped is not a cancelled request. Telling the model
    /// otherwise sends it a note about a task it never had, and the note says
    /// to treat that phantom request as cancelled.
    #[tokio::test]
    async fn a_stopped_bash_does_not_tell_the_model_a_request_was_cancelled() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let stopped = |kind: super::Kind| {
            let dir = dir.path().to_path_buf();
            async move {
                let mut tui = surface(&dir);
                let mut session = agent::session::Session::new();
                session.prompt("the task the user actually asked for");
                tui.settle(super::Done {
                    lane: 0,
                    kind,
                    ran: Some((session, Err(agent::AgentError::Cancelled))),
                })
                .await;
                let mut back = tui.core.lanes[0].session.take().expect("the transcript back");
                back.send_prompt(String::from("now something else"), None::<String>);
                format!("{:?}", back.entries())
            }
        };

        let after_bash = stopped(super::Kind::Bash(vec!["some output".into()])).await;
        assert!(
            !after_bash.contains("stopped the previous run"),
            "a stopped `!` is the user's own command, not a request the model owes: {after_bash}"
        );

        // The turn it was borrowed from still says so, or the fix went too far.
        let after_turn = stopped(super::Kind::Turn).await;
        assert!(
            after_turn.contains("stopped the previous run"),
            "a stopped turn still has to be named: {after_turn}"
        );
    }

    /// The bar reads as a row of names, not a scatter: the sign column holds
    /// only what a lane is doing — the one in front says so in colour, and
    /// never with the input prompt's own `\u{203a}`.
    #[test]
    fn the_lane_bar_separates_names_the_way_every_other_line_does() {
        let mut ui = test_ui(80, 24);
        ui.tabs = vec![
            super::Tab { mark: super::Mark::Front, name: "pi-rs".into() },
            super::Tab { mark: super::Mark::Idle, name: "f1".into() },
            super::Tab { mark: super::Mark::Done, name: "f2".into() },
        ];
        let plain = crate::render::strip_ansi(&ui.lane_bar(80).expect("two lanes make a bar"));
        assert_eq!(plain, "pi-rs \u{b7} f1 \u{b7} \u{2713} f2");

        // Wide enough for the names, far too narrow once escapes are counted
        // as columns — the whole row still has to survive.
        let narrow = crate::render::strip_ansi(&ui.lane_bar(30).expect("a bar"));
        assert!(!narrow.contains('\u{2026}'), "clipped a row that fits: {narrow}");
    }

    /// The input prompt's icon belongs to the line you type on. The bar sat
    /// directly under it wearing the same mark, which read as a second place
    /// to type — whatever the theme sets that icon to.
    #[test]
    fn the_lane_bar_never_wears_the_input_prompt() {
        let icon = crate::render::Theme::default().prompt.icon;
        let mut ui = test_ui(80, 24);
        ui.tabs = vec![
            super::Tab { mark: super::Mark::Front, name: "pi-rs".into() },
            super::Tab { mark: super::Mark::Idle, name: "f1".into() },
        ];
        let plain = crate::render::strip_ansi(&ui.lane_bar(80).expect("two lanes make a bar"));
        assert!(
            !plain.contains(&icon),
            "the bar wears the prompt icon `{icon}`: {plain}"
        );
    }

    /// The input line is the bottom row, and the bar sits above it as the
    /// edge of the history — not hanging off the line being typed, where it
    /// read as a second prompt.
    #[test]
    fn the_bar_sits_above_the_input_line() {
        let mut ui = test_ui(40, 8);
        ui.tabs = vec![
            super::Tab { mark: super::Mark::Front, name: "pi-rs".into() },
            super::Tab { mark: super::Mark::Idle, name: "f1".into() },
        ];
        let (_dir, mut lane) = a_running_lane();
        ui.flush(&mut lane);

        let rows = ui.screen.painted();
        let icon = crate::render::Theme::default().prompt.icon;
        let last = rows.last().expect("a drawn frame");
        assert!(last.starts_with(&icon), "the input line is the bottom row: {last:?}");
        let above = &rows[rows.len() - 2];
        assert_eq!(above.trim(), "pi-rs \u{b7} f1", "the bar is the row above it");
    }

    /// A flash answers the keypress on the bar row and leaves no trace in the
    /// transcript: it outranks the lane strip while it is up, and the strip
    /// comes back on its own once the window passes — with no help from
    /// whoever set the flash, who is long gone by then.
    #[test]
    fn a_flash_takes_the_bar_row_and_gives_it_back() {
        let mut ui = test_ui(40, 8);
        ui.tabs = vec![
            super::Tab { mark: super::Mark::Front, name: "pi-rs".into() },
            super::Tab { mark: super::Mark::Idle, name: "f1".into() },
        ];
        let (_dir, mut lane) = a_running_lane();
        let before = lane.view.scrollback.len();

        ui.flash("the only checkout there is");
        ui.flush(&mut lane);
        let painted = ui.screen.painted();
        let bar = &painted[painted.len() - 2];
        assert_eq!(bar.trim(), "the only checkout there is", "{painted:?}");
        assert_eq!(
            lane.view.scrollback.len(),
            before,
            "a flash is not part of the transcript"
        );

        // Backdated past the window: the next frame is the one that drops it,
        // which is what an idle screen relies on.
        let (text, _) = ui.flash.take().expect("a flash is up");
        ui.flash = Some((text, super::Instant::now().checked_sub(super::FLASH).expect("a clock")));
        ui.flush(&mut lane);
        let painted = ui.screen.painted();
        assert_eq!(painted[painted.len() - 2].trim(), "pi-rs \u{b7} f1", "{painted:?}");
        assert!(ui.flash.is_none(), "the expired flash was dropped");
    }

    /// The same notice landing again with nothing between it and the last one
    /// is one row and a count — a screenful of identical lines is the failure
    /// this stops, and only an unbroken run folds.
    #[test]
    fn the_same_notice_twice_running_is_one_row_and_a_count() {
        let mut ui = test_ui(40, 8);
        let (_dir, mut lane) = a_running_lane();
        lane.view.scrollback.clear();
        let shown = |ui: &super::Ui, lane: &crate::lane::Lane, i: usize| {
            let (text, _) = lane.view.scrollback[i].line(0, &ui.paint, &[], 80);
            crate::render::strip_ansi(&text)
        };

        ui.say(&mut lane.view, "nothing to rewind to");
        ui.say(&mut lane.view, "nothing to rewind to");
        ui.say(&mut lane.view, "nothing to rewind to");
        assert_eq!(lane.view.scrollback.len(), 1);
        assert_eq!(shown(&ui, &lane, 0), "nothing to rewind to \u{d7}3");

        // Broken by another line, the next repeat starts its own row rather
        // than reaching back over it.
        ui.say(&mut lane.view, "stopped");
        ui.say(&mut lane.view, "nothing to rewind to");
        assert_eq!(lane.view.scrollback.len(), 3);
        assert_eq!(shown(&ui, &lane, 2), "nothing to rewind to");
    }

    /// `ctrl+o` walks the checkouts in a ring. Every checkout on disk is in
    /// it, not only the open ones — the main one first, because that is the
    /// order `worktree::list` reports and a lane in it carries no name.
    #[test]
    fn ctrl_o_walks_to_the_next_checkout_and_wraps() {
        let ring = |at: Option<&str>| {
            let ui = test_ui(80, 24);
            let trees = ["pi-rs", "f1", "f2"]
                .iter()
                .map(|n| crate::repl::Choice { name: n.to_string(), note: String::new() })
                .collect();
            ui.lists.worktrees.set(trees).ok();
            let (_dir, mut lane) = a_running_lane();
            lane.worktree = at.map(str::to_string);
            ui.next_checkout(&lane)
        };
        // The main checkout is the one a lane names as None.
        assert_eq!(ring(None).as_deref(), Some("f1"));
        assert_eq!(ring(Some("f1")).as_deref(), Some("f2"));
        // And round the end, back to the main one.
        assert_eq!(ring(Some("f2")).as_deref(), Some("pi-rs"));
    }

    /// Nowhere to go is said, not walked to: one checkout has no next.
    #[test]
    fn a_lone_checkout_has_no_next() {
        let ui = test_ui(80, 24);
        ui.lists
            .worktrees
            .set(vec![crate::repl::Choice { name: "pi-rs".into(), note: String::new() }])
            .ok();
        let (_dir, lane) = a_running_lane();
        assert_eq!(ui.next_checkout(&lane), None);
    }

    /// The view travels with its lane, but the editor is the surface's, so a
    /// draft would outlive the switch. `leave_lane` is where every switch
    /// passes and has to drop it: left standing, the next Enter files one
    /// lane's half-typed prompt into the session it landed on.
    #[test]
    fn switching_checkouts_does_not_carry_a_draft_across() {
        let mut ui = test_ui(80, 24);
        ui.editor.set_line("half a thought meant for this lane");

        ui.leave_lane();

        assert_eq!(ui.editor.text(), "", "the draft stays with the lane it was typed at");
    }

    /// And the key that asks for the switch still names the right destination.
    #[test]
    fn ctrl_o_asks_for_the_next_checkout() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut ui = test_ui(80, 24);
        let trees = ["pi-rs", "f1"]
            .iter()
            .map(|n| crate::repl::Choice { name: n.to_string(), note: String::new() })
            .collect();
        ui.lists.worktrees.set(trees).ok();

        let ctrl_o = super::TermEvent::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        let (_dir, mut lane) = a_running_lane();
        let intent = ui.key(&mut lane, ctrl_o, false);
        assert!(
            matches!(&intent, crate::repl::Intent::Worktree(name) if name == "f1"),
            "{intent:?}"
        );
    }

    /// The completion list is off during a run, and the reason is not cosmetic:
    /// `When::Menu` outranks `When::Run`, so a live menu takes Esc away from
    /// `run.interrupt` and leaves the turn with no way to be stopped.
    #[test]
    fn esc_still_interrupts_a_run_with_a_command_word_in_the_editor() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut ui = test_ui(80, 24);
        ui.commands = std::sync::Arc::new(vec![crate::repl::Command {
            word: "/new".into(),
            args: "",
            help: "a fresh session".into(),
            source: crate::repl::Source::Builtin,
        }]);
        ui.editor.set_line("/new");
        let (_dir, mut lane) = a_running_lane();

        // The same line does offer a completion when nothing is running, so
        // this test fails if the guard goes rather than passing vacuously.
        assert!(!ui.menu(false).is_empty());
        assert!(ui.menu(true).is_empty(), "the editor is a queue during a run");

        let esc = super::TermEvent::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let intent = ui.key(&mut lane, esc, true);
        assert!(matches!(intent, crate::repl::Intent::Interrupt), "{intent:?}");
    }

    /// A surface with an in-memory screen, for the drawing tests below.
    /// The history area is what is left after the menu, the bar and the input.
    /// Measured without the bar's row, `window` is handed one row more than the
    /// frame can paint and `Rows` fills top-down — so the row that goes over
    /// the edge is the newest one, which is the one being read.
    #[test]
    fn the_newest_row_survives_a_frame_with_the_lane_bar_on_it() {
        let mut ui = test_ui(40, 8);
        ui.tabs = vec![
            super::Tab { mark: super::Mark::Front, name: "pi-rs".into() },
            super::Tab { mark: super::Mark::Idle, name: "f1".into() },
        ];
        let (_dir, mut lane) = a_running_lane();
        lane.turn = crate::lane::Turn::Idle;
        for i in 0..10 {
            lane.view.scrollback.push(Row::notice(format!("row-{i}")));
        }
        ui.flush(&mut lane);

        let painted = ui.screen.painted();
        assert!(
            painted.iter().any(|r| r.trim() == "pi-rs · f1"),
            "the bar is on this frame: {painted:?}"
        );
        assert!(
            painted.iter().any(|r| r.trim() == "row-9"),
            "the newest row was pushed off the bottom: {painted:?}"
        );
    }

    fn a_finished_run(ui: &mut super::Ui, lane: &mut crate::lane::Lane) {
        ui.on_event(lane, agent::Event::TurnStart { turn: 1 });
        ui.on_event(
            lane,
            agent::Event::Done {
                turns: 2,
                usage: brain::stream::Usage {
                    input: 8_400,
                    output: 390,
                    ..Default::default()
                },
                cost: 0.0012,
                ctx: (72_400, 114_000),
                compactions: 0,
            },
        );
    }

    fn spelled(
        rows: &[Row],
        paint: &Paint,
        done: &[crate::status::Segment],
    ) -> Vec<String> {
        ScrollbackRows::new(rows, paint, done, 80)
            .map(|(r, _)| crate::render::strip_ansi(&r))
            .collect()
    }

    /// The row a run ends on keeps its numbers, not the string they rendered
    /// to. The segment list that spells it out and the theme that paints it
    /// both outlive the run, and a string frozen at the end of it answers to
    /// neither.
    #[test]
    fn the_line_a_run_ends_on_is_respelled_from_its_numbers() {
        let mut ui = test_ui(80, 24);
        let (_dir, mut lane) = a_running_lane();
        a_finished_run(&mut ui, &mut lane);

        let rows = spelled(&lane.view.scrollback, &ui.paint, &crate::status::default_done());
        assert_eq!(
            rows.last().map(String::as_str),
            Some("2 turns · 8.4k in / 390 out · ctx 72.4k/114.0k · $0.0012")
        );

        // The same row, asked for differently. A stored string could not do
        // this, which is the whole of what changed.
        let narrowed = spelled(
            &lane.view.scrollback,
            &ui.paint,
            &[crate::status::Segment::Cost],
        );
        assert_eq!(narrowed.last().map(String::as_str), Some("$0.0012"));
    }

    /// A run that begins no turn — a `!` command — spends no tokens, and a row
    /// of dashes under it reads as a model call that cost nothing.
    #[test]
    fn a_bang_command_shows_no_token_counts() {
        let ui = test_ui(80, 24);
        let (_dir, mut lane) = a_running_lane();
        lane.view.started = Some(std::time::Instant::now());

        let live = ui.live(&lane, 10).join("\n");
        assert!(live.contains("esc to stop"), "the run is on: {live}");
        assert!(!live.contains(" in / "), "nothing was spent: {live}");
    }

    /// The live region follows the lane's turn, not the clock beside it. One
    /// field answering both meant every ending path had to put the clock back
    /// or leave a spinner running over a lane that had finished.
    #[test]
    fn the_live_region_ends_with_the_turn_and_not_with_the_clock() {
        let ui = test_ui(80, 24);
        let (_dir, mut lane) = a_running_lane();
        lane.view.started = Some(std::time::Instant::now());
        assert!(
            ui.live(&lane, 10).iter().any(|r| r.contains("esc to stop")),
            "a running lane shows the line it can be stopped from"
        );

        lane.turn = crate::lane::Turn::Idle;
        assert!(
            !ui.live(&lane, 10).iter().any(|r| r.contains("esc to stop")),
            "the clock is still set; the turn is what says the run is over"
        );
    }

    /// A surface with the modal keys on, at their defaults.
    #[test]
    fn the_editor_setting_splits_into_a_program_and_its_arguments() {
        assert_eq!(super::split_editor("nvim"), ("nvim".into(), vec![]));
        assert_eq!(
            super::split_editor("code -w"),
            ("code".into(), vec!["-w".to_string()])
        );
        // Blank is what an unset variable already filtered out; `vi` is the
        // same answer either way rather than a program named "".
        assert_eq!(super::split_editor("   "), ("vi".into(), vec![]));
    }

    /// The line can hold anything the user was about to say, and `/tmp` is
    /// shared, so the mode is part of the contract rather than a detail.
    #[test]
    fn the_scratch_file_carries_the_line_and_is_private() {
        let path = super::scratch_file("hello\nworld").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\nworld");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }
        std::fs::remove_file(&path).unwrap();
    }

    fn vim_ui() -> super::Ui {
        let mut ui = test_ui(80, 24);
        ui.set_vim(&crate::config::Vim { enabled: true, ..Default::default() });
        ui
    }

    fn typed(c: char) -> super::TermEvent {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        super::TermEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn mode(ui: &super::Ui) -> Option<crate::keys::Mode> {
        ui.vim.as_ref().map(|v| v.mode)
    }

    /// The sequence is read where unbound characters are typed, and its first
    /// half is a real `j` on a real line until the `k` arrives — nothing is
    /// held pending, so the screen is never a guess.
    #[test]
    fn jk_leaves_insert_and_takes_its_first_half_back_off_the_line() {
        let mut ui = vim_ui();
        let (_dir, mut lane) = a_running_lane();

        ui.key(&mut lane, typed('j'), false);
        assert_eq!(ui.editor.text(), "j", "a lone j is a j");
        assert_eq!(mode(&ui), Some(crate::keys::Mode::Insert));

        ui.key(&mut lane, typed('k'), false);
        assert_eq!(ui.editor.text(), "", "the j goes with the mode change");
        assert_eq!(mode(&ui), Some(crate::keys::Mode::Normal));
    }

    /// Outside the window the two characters are just two characters. Without
    /// this, a `j` typed minutes ago would still be armed.
    #[test]
    fn a_j_left_behind_does_not_arm_a_later_k() {
        let mut ui = vim_ui();
        let (_dir, mut lane) = a_running_lane();

        ui.key(&mut lane, typed('j'), false);
        let stale = super::Instant::now() - std::time::Duration::from_secs(1);
        ui.vim.as_mut().unwrap().last = Some(('j', stale));
        ui.key(&mut lane, typed('k'), false);

        assert_eq!(ui.editor.text(), "jk");
        assert_eq!(mode(&ui), Some(crate::keys::Mode::Insert));
    }

    /// A command between the halves breaks the sequence: `j`, a keystroke that
    /// means something, then `k` is two commands and a `j`, not a mode change.
    #[test]
    fn a_bound_key_between_the_halves_breaks_the_sequence() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut ui = vim_ui();
        let (_dir, mut lane) = a_running_lane();

        ui.key(&mut lane, typed('j'), false);
        let left = super::TermEvent::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        ui.key(&mut lane, left, false);
        ui.key(&mut lane, typed('k'), false);

        assert_eq!(ui.editor.text(), "kj", "the caret had moved before the k");
        assert_eq!(mode(&ui), Some(crate::keys::Mode::Insert));
    }

    /// Normal has to refuse the keys it does not bind. Without this the mode
    /// is a costume: `z` would still type a `z` and only the bound keys would
    /// behave, which is worse than no mode at all.
    #[test]
    fn an_unbound_character_types_nothing_in_normal() {
        let mut ui = vim_ui();
        let (_dir, mut lane) = a_running_lane();
        ui.editor.set_line("hello");
        ui.vim.as_mut().unwrap().mode = crate::keys::Mode::Normal;

        ui.key(&mut lane, typed('z'), false);
        assert_eq!(ui.editor.text(), "hello");

        // And the keys it does bind still command.
        ui.key(&mut lane, typed('0'), false);
        ui.key(&mut lane, typed('x'), false);
        assert_eq!(ui.editor.text(), "ello");
    }

    /// The way back, and what `a` does that `i` does not.
    #[test]
    fn i_and_a_return_to_insert_on_either_side_of_the_caret() {
        let mut ui = vim_ui();
        let (_dir, mut lane) = a_running_lane();
        ui.editor.set_line("ab");
        ui.vim.as_mut().unwrap().mode = crate::keys::Mode::Normal;

        ui.key(&mut lane, typed('0'), false);
        ui.key(&mut lane, typed('i'), false);
        assert_eq!(mode(&ui), Some(crate::keys::Mode::Insert));
        ui.key(&mut lane, typed('Z'), false);
        assert_eq!(ui.editor.text(), "Zab", "i types where the caret is");

        ui.vim.as_mut().unwrap().mode = crate::keys::Mode::Normal;
        ui.key(&mut lane, typed('0'), false);
        ui.key(&mut lane, typed('a'), false);
        ui.key(&mut lane, typed('Y'), false);
        assert_eq!(ui.editor.text(), "ZYab", "a types past it");
    }

    /// `x` and `D` delete and stay in Normal; these delete the same ranges and
    /// leave. The landing is the whole difference, so it is asserted twice.
    #[test]
    fn s_and_c_delete_their_range_and_land_in_insert() {
        let mut ui = vim_ui();
        let (_dir, mut lane) = a_running_lane();
        ui.editor.set_line("abcd");
        ui.vim.as_mut().unwrap().mode = crate::keys::Mode::Normal;

        ui.key(&mut lane, typed('0'), false);
        ui.key(&mut lane, typed('s'), false);
        assert_eq!(ui.editor.text(), "bcd");
        assert_eq!(mode(&ui), Some(crate::keys::Mode::Insert));
        ui.key(&mut lane, typed('Z'), false);
        assert_eq!(ui.editor.text(), "Zbcd", "s types where the character was");

        ui.vim.as_mut().unwrap().mode = crate::keys::Mode::Normal;
        ui.key(&mut lane, typed('0'), false);
        ui.key(&mut lane, typed('l'), false);
        ui.key(&mut lane, typed('C'), false);
        assert_eq!(ui.editor.text(), "Z");
        assert_eq!(mode(&ui), Some(crate::keys::Mode::Insert));
        ui.key(&mut lane, typed('Y'), false);
        assert_eq!(ui.editor.text(), "ZY", "C leaves the caret where it cut");
    }

    /// The mode outlives a submitted line, which is the whole reason it has to
    /// be visible: the gutter says which one is up.
    #[test]
    fn the_gutter_says_which_mode_is_up() {
        let mut ui = vim_ui();
        let insert = ui.prompt.clone();
        ui.vim.as_mut().unwrap().mode = crate::keys::Mode::Normal;
        ui.show_mode();
        assert_ne!(ui.prompt, insert);
        ui.leave_normal();
        assert_eq!(ui.prompt, insert);
    }

    /// Turning the keys off is the one thing that moves the mode without a
    /// keystroke — otherwise switching back on would land in Normal with
    /// nothing having asked to go there.
    #[test]
    fn turning_the_keys_off_drops_the_mode_rather_than_parking_it() {
        let mut ui = vim_ui();
        ui.vim.as_mut().unwrap().mode = crate::keys::Mode::Normal;

        ui.set_vim(&crate::config::Vim { enabled: false, ..Default::default() });
        assert!(ui.vim.is_none());

        ui.set_vim(&crate::config::Vim { enabled: true, ..Default::default() });
        assert_eq!(mode(&ui), Some(crate::keys::Mode::Insert));
    }

    fn test_ui(width: u16, height: u16) -> super::Ui {
        super::Ui::new(
            crate::tui::screen::Screen::test(width, height),
            std::sync::Arc::new(crate::keys::Keys::default()),
            Vec::new(),
            std::sync::Arc::new(Vec::new()),
            super::Lists::new(
                crate::session::Store::new(std::env::temp_dir()),
                std::env::temp_dir(),
            ),
            Paint::new(true),
        )
    }



    // ------------------------------------------------------------- looping

    use crate::lane::Round;

    fn wrote(lane: &mut crate::lane::Lane, name: &str) {
        lane.ctx.note_write(&lane.ctx.workspace.root().join(name));
    }

    /// The whole point of the command: what decides another round is the tree,
    /// so a pass that believes it is finished is overruled by the file it just
    /// changed.
    #[test]
    fn a_loop_goes_round_while_the_tree_keeps_changing() {
        let (_dir, mut lane) = a_running_lane();
        lane.loop_start("/code-review high".into());

        lane.loop_running();
        wrote(&mut lane, "a.rs");
        let again = lane.loop_step(true, None).expect("a loop is in force");
        assert!(
            matches!(&again, Round::Again { goal, next: 2 } if goal == "/code-review high"),
            "the goal goes back verbatim, as the round it now is",
        );

        // The same file again. What a loop like this does most of the time is
        // keep working the files it has already touched, so a record that
        // counted distinct paths would call this round idle and stop here.
        lane.loop_running();
        wrote(&mut lane, "a.rs");
        assert!(matches!(lane.loop_step(true, None), Some(Round::Again { next: 3, .. })));

        // Nothing changed: a pass with nothing to do has nothing to do next
        // time either.
        lane.loop_running();
        assert!(matches!(lane.loop_step(true, None), Some(Round::Quiet)));
        assert!(lane.looping.is_none(), "and the loop is gone");
        assert!(lane.loop_step(true, None).is_none(), "a later turn is not a round");
    }

    /// A line typed between rounds ends a turn too. Counting it would move the
    /// loop on work it never ran — and end it, if that line wrote nothing.
    #[test]
    fn a_turn_the_loop_did_not_start_is_not_one_of_its_rounds() {
        let (_dir, mut lane) = a_running_lane();
        lane.loop_start("go".into());

        // Somebody else's turn settling, mid-loop.
        assert!(lane.loop_step(true, None).is_none(), "not the loop's round");
        assert!(lane.looping.is_some(), "and the loop is untouched");
        assert_eq!(lane.looping.as_ref().map(|l| l.round), Some(0));

        lane.loop_running();
        wrote(&mut lane, "a.rs");
        assert!(matches!(lane.loop_step(true, None), Some(Round::Again { next: 2, .. })));
    }

    /// Esc is the only brake when `loop_max_turns` is unset, so it has to stop
    /// the loop and not merely the round it caught.
    #[test]
    fn a_cut_round_takes_the_loop_with_it() {
        let (_dir, mut lane) = a_running_lane();
        lane.loop_start("go".into());
        lane.loop_running();
        wrote(&mut lane, "a.rs");
        assert!(matches!(lane.loop_step(false, None), Some(Round::Cut)));
        assert!(lane.looping.is_none());
    }

    /// The ceiling exists for the one shape convergence cannot catch: a round
    /// that undoes the last one changes files forever.
    #[test]
    fn a_loop_stops_at_the_configured_ceiling_with_work_still_left() {
        let (_dir, mut lane) = a_running_lane();
        lane.loop_start("go".into());
        lane.loop_running();
        wrote(&mut lane, "a.rs");
        assert!(matches!(lane.loop_step(true, Some(1)), Some(Round::Capped(1))));
        assert!(lane.looping.is_none());

        // Unset, the same round goes on: the ceiling is a config, not a default.
        lane.loop_start("go".into());
        lane.loop_running();
        wrote(&mut lane, "b.rs");
        assert!(matches!(lane.loop_step(true, None), Some(Round::Again { .. })));
    }

    /// A round can outlive the loop that queued it — the surface drops those,
    /// and nothing about them may arm a loop that is over.
    #[test]
    fn a_loop_that_has_ended_cannot_be_revived_by_a_stale_round() {
        let (_dir, mut lane) = a_running_lane();
        lane.loop_start("go".into());
        lane.loop_running();
        assert!(matches!(lane.loop_step(true, None), Some(Round::Quiet)));

        // What a `LoopRound` still sitting in the queue would do on its way
        // through: neither of these may bring the loop back.
        lane.loop_running();
        wrote(&mut lane, "a.rs");
        assert!(lane.looping.is_none(), "no loop to mark as running");
        assert!(lane.loop_step(true, None).is_none(), "and none to step");
    }

    /// A loop whose goal is another loop would arm itself every round.
    #[test]
    fn a_loop_cannot_be_read_as_its_own_goal() {
        assert!(matches!(crate::repl::read("/loop go"), crate::repl::Intent::Loop(_)));
    }
}
