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
use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

use crate::render::{self, Paint};
use crate::repl::{Repl, Step};
use editor::Editor;
use screen::{Caret, Screen};

const DIM: &str = "\x1b[2m";
const BANNER: &str = "\x1b[2m/help for commands · esc stops a run · ctrl-c clears the line, twice \
                      quickly to quit · ctrl-d exits\x1b[0m";

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

/// Everything the terminal shows, and nothing the session knows.
struct Ui {
    screen: Screen,
    editor: Editor,
    paint: Paint,
    /// Model output with no newline after it yet. Kept live because it is still
    /// being written; a completed line goes straight to scrollback.
    open: String,
    /// Whether `open` is reasoning rather than the answer.
    dim: bool,
    /// Finished lines waiting to be pushed above on the next render.
    above: Vec<String>,
    /// Lines submitted while the run was working.
    queued: Vec<String>,
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
    stopping: bool,
}

impl Ui {
    fn new(screen: Screen) -> Self {
        Self {
            screen,
            editor: Editor::default(),
            paint: Paint { color: true },
            open: String::new(),
            dim: false,
            above: vec![BANNER.to_string()],
            queued: Vec::new(),
            last_interrupt: None,
            started: None,
            spinner: 0,
            settled: Usage::default(),
            turn: Usage::default(),
            produced: 0,
            stopping: false,
        }
    }

    fn say(&mut self, line: impl Into<String>) {
        self.above.push(line.into());
    }

    /// End the open paragraph and send it up into scrollback.
    fn close(&mut self) {
        if !self.open.is_empty() {
            let text = std::mem::take(&mut self.open);
            let painted = if self.dim {
                self.paint.on(DIM, &text)
            } else {
                text
            };
            self.above.push(painted);
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
        // terminal's own scrollback rather than in the region we repaint.
        while let Some(i) = self.open.find('\n') {
            let line: String = self.open.drain(..=i).collect();
            let line = line.trim_end_matches('\n').to_string();
            let painted = if dim { self.paint.on(DIM, &line) } else { line };
            self.above.push(painted);
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
            Event::TurnEnd { usage, .. } => {
                self.settled.input += usage.input;
                self.settled.output += usage.output;
                self.settled.cache_read += usage.cache_read;
                self.settled.cache_write += usage.cache_write;
                self.turn = Usage::default();
                self.produced = 0;
            }
            Event::TurnStart { .. } => {}
            _ => {
                self.close();
                if let Some(line) = render::describe(&event, self.paint) {
                    self.above.push(line);
                }
            }
        }
    }

    /// The rows to repaint, and where the caret sits among them.
    fn live(&self) -> (Vec<String>, Caret) {
        let width = self.screen.usable();
        let mut rows = Vec::new();

        if !self.open.is_empty() {
            let painted = if self.dim {
                self.paint.on(DIM, &self.open)
            } else {
                self.open.clone()
            };
            let mut wrapped = screen::fit(&painted, width);
            // A paragraph can outgrow the screen; the tail is the part still
            // being written, and the rest reaches scrollback when it closes.
            let room = (self.screen.height as usize).saturating_sub(4).max(1);
            if wrapped.len() > room {
                wrapped.drain(..wrapped.len() - room);
            }
            rows.extend(wrapped);
        }

        if let Some(since) = self.started {
            let line = status::line(
                self.spinner,
                since.elapsed(),
                &counts(&self.settled, &self.turn, self.produced),
                self.queued.len(),
                self.stopping,
            );
            rows.extend(screen::fit(&self.paint.on(DIM, &line), width));
        }

        let (input, caret) = self.editor.view(width);
        let offset = rows.len() as u16;
        rows.extend(input);
        let mut caret = (caret.0 + offset, caret.1);

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

    fn flush(&mut self) {
        let (live, caret) = self.live();
        let above = std::mem::take(&mut self.above);
        let _ = self.screen.render(&above, &live, caret);
    }

    /// Echo what was sent, so the prompt survives the editor being cleared.
    fn echo(&mut self, line: &str) {
        for (i, part) in line.split('\n').enumerate() {
            let gutter = if i == 0 { "\x1b[36m›\x1b[0m " } else { "  " };
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
        let KeyEvent {
            code, modifiers, ..
        } = key;
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let alt = modifiers.contains(KeyModifiers::ALT);

        match code {
            KeyCode::Char('c') if ctrl => {
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
                    self.say(self.paint.on(DIM, "press ctrl-c again to quit"));
                } else {
                    self.editor.clear();
                }
                Act::None
            }
            KeyCode::Char('d') if ctrl => {
                if self.editor.is_empty() && !running {
                    Act::Quit
                } else {
                    self.editor.delete();
                    Act::None
                }
            }
            KeyCode::Esc => {
                if running {
                    Act::Interrupt
                } else {
                    Act::None
                }
            }
            // Alt-Enter and Ctrl-J both continue the line; plain Enter sends it.
            // Shift-Enter only reaches us from terminals that speak the kitty
            // protocol, so it cannot be the only way to type a newline.
            KeyCode::Enter if alt || ctrl || modifiers.contains(KeyModifiers::SHIFT) => {
                self.editor.insert('\n');
                Act::None
            }
            KeyCode::Char('j') if ctrl => {
                self.editor.insert('\n');
                Act::None
            }
            KeyCode::Enter => {
                let line = self.editor.take();
                if line.trim().is_empty() {
                    Act::None
                } else {
                    Act::Submit(line)
                }
            }
            KeyCode::Backspace => {
                if alt || ctrl {
                    self.editor.kill_word_back();
                } else {
                    self.editor.backspace();
                }
                Act::None
            }
            KeyCode::Delete => {
                self.editor.delete();
                Act::None
            }
            KeyCode::Left if alt || ctrl => {
                self.editor.word_left();
                Act::None
            }
            KeyCode::Right if alt || ctrl => {
                self.editor.word_right();
                Act::None
            }
            KeyCode::Left => {
                self.editor.left();
                Act::None
            }
            KeyCode::Right => {
                self.editor.right();
                Act::None
            }
            KeyCode::Up => {
                self.editor.up();
                Act::None
            }
            KeyCode::Down => {
                self.editor.down();
                Act::None
            }
            KeyCode::Home => {
                self.editor.home();
                Act::None
            }
            KeyCode::End => {
                self.editor.end();
                Act::None
            }
            KeyCode::Char('a') if ctrl => {
                self.editor.home();
                Act::None
            }
            KeyCode::Char('e') if ctrl => {
                self.editor.end();
                Act::None
            }
            KeyCode::Char('k') if ctrl => {
                self.editor.kill_to_end();
                Act::None
            }
            KeyCode::Char('u') if ctrl => {
                self.editor.kill_to_start();
                Act::None
            }
            KeyCode::Char('w') if ctrl => {
                self.editor.kill_word_back();
                Act::None
            }
            KeyCode::Char('l') if ctrl => {
                self.screen.clear();
                Act::None
            }
            KeyCode::Char('b') if alt => {
                self.editor.word_left();
                Act::None
            }
            KeyCode::Char('f') if alt => {
                self.editor.word_right();
                Act::None
            }
            // Alt excluded too: an unbound Alt-chord is a chord, not the letter
            // it was pressed with.
            KeyCode::Char(c) if !ctrl && !alt => {
                self.editor.insert(c);
                Act::None
            }
            _ => Act::None,
        }
    }
}

/// What the status line should say the run has cost.
///
/// The turn in flight contributes only what the provider has already stated —
/// the input count on the Anthropic wire, nothing at all on the OpenAI one —
/// so its output is stood in for by the bytes that have arrived. Its measured
/// figures are replaced, never added to, when its `TurnEnd` folds them into
/// `settled`, or a turn's input would be counted twice.
fn counts(settled: &Usage, turn: &Usage, produced: usize) -> status::Counts {
    let exact = turn.output > 0;
    status::Counts {
        input: settled.input + turn.input,
        output: settled.output
            + if exact {
                turn.output
            } else {
                brain::estimate::bytes(produced) as u64
            },
        exact,
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
    pub fn new(core: Repl) -> Result<Self> {
        let mut ui = Ui::new(Screen::new()?);
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
                Step::Handled(lines) => self.ui.above.extend(lines),
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
        self.ui.started = None;

        // Saved either way: an interrupted turn is exactly the one worth keeping.
        if let Err(e) = self.core.store.save(
            &self.core.id,
            self.core.ctx.workspace.root(),
            &self.core.model,
            &self.core.session.log,
        ) {
            self.ui
                .say(format!("warning: the transcript was not saved: {e}"));
        }

        match out {
            Ok(o) => self.totals.add(&o.totals.usage, o.totals.cost),
            Err(AgentError::Cancelled) => self.ui.say(self.ui.paint.on(DIM, "stopped")),
            Err(e) => {
                let text = format!("\x1b[31merror\x1b[0m {e}");
                self.ui.say(text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::counts;
    use brain::stream::Usage;

    #[test]
    fn a_turn_that_has_only_started_shows_its_input_and_guesses_its_output() {
        let turn = Usage {
            input: 8_400,
            ..Default::default()
        };
        let c = counts(&Usage::default(), &turn, 1_536);
        assert_eq!((c.input, c.output, c.exact), (8_400, 512, false));
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
        let c = counts(&settled, &turn, 300);
        assert_eq!((c.input, c.output, c.exact), (12_000, 700, false));
    }

    #[test]
    fn a_measured_output_supersedes_the_guess_for_the_same_turn() {
        let turn = Usage {
            input: 2_000,
            output: 90,
            ..Default::default()
        };
        let c = counts(&Usage::default(), &turn, 9_000);
        assert_eq!((c.output, c.exact), (90, true));
    }
}
