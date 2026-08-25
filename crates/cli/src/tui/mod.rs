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

use std::time::Instant;

use agent::{AgentError, Event, Totals};
use anyhow::Result;
use brain::stream::Usage;
use crossterm::event::{Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::keys::{Action, Keys, Press};
use crate::render::{self, Markdown, Paint};
use crate::repl::{self, Candidate, Choice, Command, Repl, Step};
use editor::Editor;
use screen::{Caret, Screen};
use std::sync::Arc;

/// What a folded run shows instead of what it is thinking.
const THINKING: &str = "thinking...";

const BANNER: &str = "/help for commands · esc stops a run · ctrl-c clears the line, twice \
                      quickly to quit · ctrl-d exits";

/// How close two Ctrl-C presses must be to read as one deliberate quit.
///
/// Borrowed from pi, which uses the same 500ms. A latching flag looks simpler
/// and is wrong: clear one half-typed line, type another, clear that — and the
/// second clear reads as the second half of a double-tap and quits.
const DOUBLE_TAP: std::time::Duration = std::time::Duration::from_millis(500);

fn is_double_tap(previous: Option<Instant>, now: Instant) -> bool {
    previous.is_some_and(|p| now.duration_since(p) < DOUBLE_TAP)
}

/// What a key press asked the loop to do. Every press redraws regardless.
#[derive(Debug, PartialEq, Eq)]
enum Act {
    None,
    Submit(String),
    Interrupt,
    Quit,
}

/// Whether reasoning reaches the scrollback, and what it holds back while it
/// does not.
///
/// A switch, set before or during a block and never after: a line handed to the
/// terminal cannot be taken back, and a key that pretended otherwise would put
/// reasoning below the answer it came before. Shut, the block leaves a count
/// and the lines are dropped; open, they are ordinary output.
struct Thinking {
    /// Reasoning lines the shut window has kept out of scrollback.
    held: Vec<String>,
    /// Whether reasoning is kept back at all.
    folded: bool,
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
            held: Vec::new(),
            folded: true,
        }
    }
}

impl Thinking {
    /// Whether a finished row waits here instead of going to scrollback.
    fn holds(&self, dim: bool) -> bool {
        self.folded && dim
    }

    /// Open the window or shut it, and say what the scrollback gets for it.
    ///
    /// A line naming the new state, always: pressed between runs the switch has
    /// nothing to hand over and only governs the next block, so without one the
    /// key would answer nothing at all. Then what was held — the answer has not
    /// started, so those lines still land where they belong, before it.
    /// Shutting takes nothing back, and starts at the next line: the terminal
    /// owns what it has printed, which is why this is a switch and not a view.
    fn toggle(&mut self) -> (String, Vec<String>) {
        self.folded = !self.folded;
        if self.folded {
            ("thinking · folded".to_string(), Vec::new())
        } else {
            (
                "thinking · shown".to_string(),
                std::mem::take(&mut self.held),
            )
        }
    }

    /// This block of reasoning is over. A count is what the shut window leaves,
    /// because the point of shutting it is that the scrollback does not keep
    /// the rest — and the lines go, so nothing outlives the count that stands
    /// for it.
    fn close(&mut self) -> Option<String> {
        let n = std::mem::take(&mut self.held).len();
        if !self.folded || n == 0 {
            return None;
        }
        let s = if n == 1 { "" } else { "s" };
        Some(format!("thinking · {n} line{s}"))
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
    md: &Markdown,
    dim: bool,
    open: &str,
    width: usize,
    room: usize,
    paint: &Paint,
) -> Vec<String> {
    // One row stands for all of it, the unfinished line included.
    if thinking.holds(dim) {
        return vec![paint.on(&paint.theme.muted, THINKING)];
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

/// Everything the terminal shows, and nothing the session knows.
struct Ui {
    screen: Screen,
    keys: Arc<Keys>,
    editor: Editor,
    paint: Paint,
    /// The painted prompt gutter, shared by the editor and the echoed lines.
    prompt: String,
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
    above: Vec<String>,
    /// Tool calls still running, one animated row each. A finished call
    /// replaces its row with the ✓/✗ line in scrollback, so a call that never
    /// answered would leave a spinning row behind; `abandon_tools` clears it.
    tools: Vec<RunTool>,

    /// Lines submitted while the run was working.
    queued: Vec<String>,
    /// Which completion is highlighted. Kept rather than the list itself: the
    /// list is a function of what has been typed, and caching it is one more
    /// thing to invalidate.
    picked: usize,
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
}

impl Ui {
    fn new(
        screen: Screen,
        keys: Arc<Keys>,
        choices: Vec<Choice>,
        commands: Arc<Vec<Command>>,
        paint: Paint,
    ) -> Self {
        let prompt = Self::paint_prompt(&paint);
        let banner = paint.on(&paint.theme.muted, BANNER);
        let mut editor = Editor::default();
        editor.set_prompt(prompt.clone());
        Self {
            screen,
            keys,
            choices,
            commands,
            editor,
            paint,
            prompt,
            open: String::new(),
            dim: false,
            md: Markdown::default(),
            thinking: Thinking::default(),
            above: vec![banner],
            tools: Vec::new(),
            queued: Vec::new(),
            picked: 0,
            dismissed_at: None,
            last_interrupt: None,
            started: None,
            spinner: 0,
            settled: Usage::default(),
            turn: Usage::default(),
            produced: 0,
            estimated: agent::Estimated::default(),
            stopping: false,
        }
    }

    /// The prompt gutter as the terminal shows it, colour and all.
    fn paint_prompt(paint: &Paint) -> String {
        format!(
            "{} ",
            paint.on(&paint.theme.prompt.color, &paint.theme.prompt.icon)
        )
    }
    fn say(&mut self, line: impl Into<String>) {
        self.above.push(line.into());
    }

    /// Where a finished row goes: the terminal's scrollback, or the window that
    /// can still be redrawn. The only rows that wait are held-back reasoning.
    fn land(&mut self, painted: String, dim: bool) {
        if self.thinking.holds(dim) {
            self.thinking.held.push(painted);
        } else {
            self.above.push(painted);
        }
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
            // The reasoning is over, so the scrollback finally gets its say in
            // it: the lines if the window was open, a count of them if not.
            self.close_thinking();
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
        }
        self.open.push_str(delta);
        // A finished line is no longer changing, so it belongs in the
        // terminal's own scrollback rather than in the region we repaint —
        // unless it is reasoning, which waits until its block ends for a key
        // that can still decide where it goes.
        while let Some(i) = self.open.find('\n') {
            let line: String = self.open.drain(..=i).collect();
            let line = line.trim_end_matches('\n').to_string();
            let painted = self.paint_row(&line, dim);
            self.land(painted, dim);
        }
    }

    /// What the scrollback keeps once a block of reasoning is over.
    fn close_thinking(&mut self) {
        if let Some(note) = self.thinking.close() {
            self.above
                .push(self.paint.on(&self.paint.theme.muted, &note));
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
                    self.above.extend(said.lines().map(str::to_string));
                }
            }
            _ => {
                self.close();
                if let Some(said) = render::describe(&event, &self.paint, self.screen.usable()) {
                    // Row by row: a scrollback line is written with a carriage
                    // return of its own, and an embedded newline would stair-
                    // step down the screen without one.
                    self.above.extend(said.lines().map(str::to_string));
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
            let arg = if t.summary.is_empty() {
                String::new()
            } else {
                format!(" {}", t.summary)
            };
            let line = format!("→ {}{arg}", t.name);
            self.say(self.paint.on(&self.paint.theme.muted, &line));
        }
    }

    /// What the line could still become. Only while a command word is being
    /// typed, and never during a run — the editor is a queue then, not a
    /// command line.
    fn menu(&self) -> Vec<Candidate> {
        if self.started.is_some() || self.dismissed_at.as_deref() == Some(self.editor.text()) {
            return Vec::new();
        }
        repl::complete(self.editor.text(), &self.commands, &self.choices)
    }

    /// The highlighted completion, clamped: the list shrinks as the word grows.
    fn highlighted(&self) -> Option<Candidate> {
        let mut menu = self.menu();
        if menu.is_empty() {
            return None;
        }
        Some(menu.swap_remove(self.picked.min(menu.len() - 1)))
    }

    fn menu_rows(&self, width: usize) -> Vec<String> {
        let menu = self.menu();
        if menu.is_empty() {
            return Vec::new();
        }
        let picked = self.picked.min(menu.len() - 1);
        let head = menu.iter().map(|c| c.show.len()).max().unwrap_or(0);
        menu.iter()
            .enumerate()
            .map(|(i, c)| {
                let line = format!("  {:head$}  {}", c.show, c.help);
                let painted = if i == picked {
                    self.paint.on(&self.paint.theme.menu.selected, &line)
                } else {
                    self.paint.on(&self.paint.theme.muted, &line)
                };
                screen::fit(&painted, width)
            })
            .map(|rows| rows.into_iter().next().unwrap_or_default())
            .collect()
    }

    /// The rows to repaint, and where the caret sits among them.
    fn live(&self) -> (Vec<String>, Caret) {
        let width = self.screen.usable();
        let mut rows = Vec::new();

        for t in &self.tools {
            let line = tool_row(self.spinner, &t.name, &t.summary);
            rows.extend(screen::fit(
                &self.paint.on(&self.paint.theme.muted, &line),
                width,
            ));
        }

        let room = (self.screen.height as usize).saturating_sub(4).max(1);
        rows.extend(body(
            &self.thinking,
            &self.md,
            self.dim,
            &self.open,
            width,
            room,
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

        let (input, caret) = self.editor.view(width);
        let offset = rows.len() as u16;
        rows.extend(input);
        let mut caret = (caret.0 + offset, caret.1);
        // Below the prompt, so the line being typed does not move under the
        // cursor as the list grows and shrinks with it.
        rows.extend(self.menu_rows(width));

        // A region taller than the screen scrolls its own top rows away, and
        // the walk back over it then eats lines that are no longer part of it.
        // Drop from the top instead, keeping the row the caret is on.
        let room = (self.screen.height as usize).saturating_sub(1).max(1);
        if rows.len() > room {
            let start = (caret.0 as usize + 1)
                .saturating_sub(room)
                .min(rows.len() - room);
            rows.drain(..start);
            rows.truncate(room);
            caret.0 -= start as u16;
        }
        (rows, caret)
    }

    fn set_theme(&mut self, theme: Arc<render::Theme>) {
        self.paint.theme = theme;
        self.prompt = Self::paint_prompt(&self.paint);
        self.editor.set_prompt(self.prompt.clone());
        // above[0] is the banner, painted once at construction; restyle it so
        // a /reload lands on the new theme instead of the old.
        if let Some(first) = self.above.first_mut() {
            *first = self.paint.on(&self.paint.theme.muted, BANNER);
        }
    }

    fn flush(&mut self) {
        let (live, caret) = self.live();
        let above = std::mem::take(&mut self.above);
        let _ = self.screen.render(&above, &live, caret);
    }

    /// Echo what was sent, so the prompt survives the editor being cleared.
    fn echo(&mut self, line: &str) {
        for (i, part) in line.split('\n').enumerate() {
            let gutter = if i == 0 {
                self.prompt.clone()
            } else {
                "  ".to_string()
            };
            self.say(format!("{gutter}{part}"));
        }
    }

    fn key(&mut self, event: TermEvent, running: bool) -> Act {
        let key = match event {
            TermEvent::Resize(w, h) => {
                self.screen.resized(w, h);
                return Act::None;
            }
            TermEvent::Paste(text) => {
                self.editor.insert_str(&text.replace('\r', "\n"));
                return Act::None;
            }
            // Windows reports both press and release; acting on each would
            // double every keystroke.
            TermEvent::Key(k) if k.kind != KeyEventKind::Release => k,
            _ => return Act::None,
        };
        let press = Press::of(key.code, key.modifiers);
        let bound = self.keys.action(press, !self.menu().is_empty(), running);

        match bound {
            Some(Action::LineClear) => return self.interrupt_or_clear(running),
            Some(Action::LineSubmit) => {
                // Enter while the menu is open runs what it highlights. The
                // typed text is a prefix; the highlighted word is the intent.
                // The menu reads the editor, so pick before draining it.
                let picked = self.highlighted();
                let typed = self.editor.take();
                let line = match picked {
                    Some(c) => c.line,
                    None => typed,
                };
                return if line.trim().is_empty() {
                    Act::None
                } else {
                    Act::Submit(line)
                };
            }
            Some(Action::RunInterrupt) => return Act::Interrupt,
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
            Some(Action::ThinkFold) => {
                let (said, mut held) = self.thinking.toggle();
                self.above
                    .push(self.paint.on(&self.paint.theme.muted, &said));
                self.above.append(&mut held);
            }
            Some(Action::MenuAccept) => {
                if let Some(c) = self.highlighted() {
                    self.editor.set_line(&c.line);
                    // Something still expected after it wants a space first.
                    if c.more {
                        self.editor.insert(' ');
                    }
                    self.picked = 0;
                }
            }
            Some(Action::MenuNext) => {
                let n = self.menu().len();
                self.picked = (self.picked + 1).min(n.saturating_sub(1));
            }
            Some(Action::MenuPrevious) => self.picked = self.picked.saturating_sub(1),
            Some(Action::MenuDismiss) => {
                // Recorded against the text, so any edit brings the list back:
                // this means "not that", not "never again".
                self.dismissed_at = Some(self.editor.text().to_string());
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
                Action::LineClear | Action::LineSubmit | Action::RunInterrupt | Action::AppExit,
            ) => unreachable!("handled above"),
        }
        Act::None
    }

    /// One key, three meanings, and the escalation travels with the binding
    /// rather than with Ctrl-C: stop the run, clear the line, or — pressed
    /// twice inside the window — leave.
    fn interrupt_or_clear(&mut self, running: bool) -> Act {
        let now = Instant::now();
        let quit = is_double_tap(self.last_interrupt, now);
        self.last_interrupt = Some(now);
        if quit {
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
    crate::session::state_dir().map(|d| d.join("history"))
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
                Act::Interrupt | Act::None => continue,
            };
            self.ui.echo(&line);
            // Written per line rather than on the way out: quitting with two
            // Ctrl-Cs skips every tidy exit path there is.
            self.save_history();
            match self.core.command(&line, &self.totals) {
                Step::Quit => break,
                Step::Bash(command) => {
                    self.ui.above.extend(self.core.bash(&command).await);
                }
                Step::Handled(lines) => {
                    self.ui.above.extend(lines);
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
                            self.ui.echo(&queued);
                            next = Some(queued);
                        }
                    }
                }
            }
        }
        self.save_history();
        Ok(())
    }

    async fn turn(
        &mut self,
        prompt: String,
        tx: &UnboundedSender<Event>,
        rx: &mut UnboundedReceiver<Event>,
    ) {
        self.core.session.log.resume(prompt);
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
    use super::{Thinking, body, counts, tool_row};
    use crate::render::Markdown;
    use crate::render::Paint;
    use brain::stream::Usage;

    fn thought(n: usize) -> Thinking {
        let mut t = Thinking::default();
        for i in 1..=n {
            t.held.push(format!("line {i}"));
        }
        t
    }

    /// What the screen shows for a run in the middle of reasoning.
    fn shown(t: &Thinking, open: &str) -> Vec<String> {
        body(
            t,
            &Markdown::default(),
            true,
            open,
            80,
            9,
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
    fn a_shut_window_is_one_row_whatever_it_holds() {
        // Including the line still arriving: it is reasoning too, and putting
        // it on screen is the window this row exists to replace.
        let t = thought(30);
        assert_eq!(shown(&t, "half a sentence"), vec!["thinking..."]);
    }

    #[test]
    fn opening_it_hands_the_scrollback_what_was_held() {
        // The answer has not started, so they still land where they belong.
        let mut t = thought(5);
        let (said, held) = t.toggle();
        assert_eq!(said, "thinking · shown");
        assert_eq!(held.len(), 5);
        assert!(t.held.is_empty());
        assert!(!t.holds(true), "open, reasoning is ordinary output");
        assert_eq!(shown(&t, "half a sentence"), vec!["half a sentence"]);
    }

    #[test]
    fn shutting_it_again_starts_from_the_next_line() {
        // What already reached the terminal stays there; a switch that
        // pretended otherwise would have to never print in the first place.
        let mut t = thought(5);
        t.toggle();
        let (said, held) = t.toggle();
        assert_eq!(said, "thinking · folded");
        assert!(held.is_empty());
        assert!(t.holds(true));
    }

    #[test]
    fn the_answer_is_never_held() {
        let t = thought(3);
        assert!(!t.holds(false));
        let rows = body(
            &t,
            &Markdown::default(),
            false,
            "hello",
            80,
            9,
            &Paint::new(false),
        );
        assert_eq!(rows, vec!["hello"]);
    }

    #[test]
    fn a_shut_window_leaves_a_count_and_nothing_behind_it() {
        // Nothing outlives the count that stands for it: a line kept back here
        // could only ever be printed below the answer it came before.
        let mut t = thought(13);
        assert_eq!(t.close().as_deref(), Some("thinking · 13 lines"));
        assert!(t.held.is_empty());
        assert_eq!(t.close(), None, "nothing held, nothing said");
        assert!(t.toggle().1.is_empty(), "and nothing left to open");

        let mut one = thought(1);
        assert_eq!(one.close().as_deref(), Some("thinking · 1 line"));
    }

    #[test]
    fn an_open_window_leaves_no_count() {
        // It was printed as it arrived; a count would be describing what the
        // reader is already looking at.
        let mut open = thought(4);
        open.toggle();
        assert_eq!(open.close(), None);
    }

    #[test]
    fn a_long_paragraph_is_trimmed_to_the_room_it_is_given() {
        let t = Thinking::default();
        let rows = body(
            &t,
            &Markdown::default(),
            false,
            &"x".repeat(500),
            80,
            3,
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
}
