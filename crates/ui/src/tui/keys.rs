//! Where a press goes: the key table read against the line, a list or a
//! page over it — and the mouse.
use super::DOUBLE_TAP;
use super::menu::MenuEntry;
use super::ui::LastPress;
use super::view::{Doing, View};
use super::{Asked, Deed, Ui};
use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use pi_core::core::lane::Lane;
use pi_core::input::Builtin;
use pi_core::input::commands::Acceptance;
use pi_core::input::{self, Intent};
use pi_store::keys::{Action, Key, Layers, Mode, Mods, Press, Surface};
use std::time::Instant;

/// The word that lists what every key does.
pub(super) const KEYS: &str = "/keys";

/// The words this surface answers itself, listed beside the built-ins.
pub const WORDS: &[input::commands::Command] = &[input::commands::Command::surface(
    KEYS,
    input::commands::Args::None,
    "what every key does, and the id to rebind it under",
)];

impl Ui {
    // The line submitted since this was last asked, if any: the surface
    // catches the recall list up and carries the line to its answer.
    pub(super) fn took_submit(&mut self) -> Option<String> {
        std::mem::take(&mut self.submitted)
    }

    // A page up over the transcript, or a search on it.
    pub(super) fn reading(&self) -> bool {
        self.read.is_some() || self.search.is_some()
    }

    // The first question let go, answered with the options at `picks` — the
    // words that narrowed to them go too — or with the person's `own` words;
    // with neither, closed unanswered. Picks that cannot go together leave
    // it up, saying why.
    fn answer(&mut self, picks: &[usize], own: Option<String>) -> Asked {
        self.disarm();
        let Some(question) = self.questions.first().cloned() else {
            return Asked::Own(Deed::Nothing);
        };
        let picked = match question.picked(picks) {
            Ok(picked) => picked,
            Err(why) => {
                self.flash(why);
                return Asked::Own(Deed::Nothing);
            }
        };
        if picked.is_some() {
            self.editor.clear();
        }
        self.sync_questions(self.questions[1..].to_vec());
        tracing::info!(
            target: "pi::session", question = question.id, from = ?question.from, ?picks,
            own = own.is_some(), "question answered"
        );
        if !question.back {
            self.answered.push(question.id);
        }
        match (question.back, own.or(picked)) {
            (true, picked) => Asked::Own(Deed::Answer {
                question: question.id,
                picked,
            }),
            (false, Some(line)) => self.run_line(line),
            (false, None) => Asked::Own(Deed::Nothing),
        }
    }

    pub(super) fn key(
        &mut self,
        lane: &Lane,
        view: &mut View,
        event: TermEvent,
        running: bool,
    ) -> Asked {
        let key = match event {
            TermEvent::Resize(w, h) => {
                self.screen.resized(w, h);
                return Asked::Own(Deed::Nothing);
            }
            TermEvent::Paste(text) => {
                if self.background_focus.is_some() {
                    return Asked::Own(Deed::Nothing);
                }
                self.last_press = None;
                // Only the line or a query being typed takes text: anywhere
                // else the paste would land where nobody can see it.
                if self.search_typing() {
                    self.search_paste(&text);
                } else {
                    self.editor.insert_str(&text.replace('\r', "\n"));
                }
                return Asked::Own(Deed::Nothing);
            }
            TermEvent::Mouse(mouse) => {
                if matches!(mouse.kind, MouseEventKind::Down(_)) {
                    self.leave_background();
                }
                return self.mouse(view, mouse);
            }
            // Windows reports both press and release; acting on both would
            // double every keystroke.
            TermEvent::Key(k) if k.kind != KeyEventKind::Release => {
                self.selection = None;
                k
            }
            _ => return Asked::Own(Deed::Nothing),
        };
        let press = press_of(&key);
        let now = Instant::now();
        // Read against what is shown, a held character still off the list.
        let layers = self.layers(running);
        let last = self.last_press.take();
        let prev = last
            .filter(|l| now.duration_since(l.at) < self.pair_window_after(l.press))
            .map(|l| l.press);
        let hit = self.keys.hit(prev, press, layers);
        if let Some(h) = hit
            && h.pair
        {
            // The pair's first half typed itself onto the line before anyone
            // knew it was one; take it back, so the line shows what it means.
            if last.is_some_and(|l| l.typed) {
                self.editor.backspace();
            }
        } else {
            // A finished pair starts none: `g g g` is a pair and a `g`.
            self.last_press = Some(LastPress {
                press,
                at: now,
                typed: false,
                opens: self.keys.opens_pair(press, layers),
            });
        }
        let action = hit.map(|h| h.action);

        match action {
            Some(Action::AppCancel) => return self.cancel(view, running),
            Some(Action::AppQuit) => return Asked::Core(Intent::Builtin(Builtin::Quit)),
            _ => {}
        }
        if let Some(asked) = self.background_key(key) {
            return asked;
        }
        if self.search_key(view, action) {
            self.disarm();
            return Asked::Own(Deed::Nothing);
        }
        let was = self.search_typing().then(|| self.editor.text().to_string());
        let asked = self.editor_key(lane, view, key, action);
        if let Some(was) = was {
            self.search_typed(&was);
        }
        asked
    }

    // The layers the key table reads: the line's, a list or a page over it.
    fn layers(&mut self, running: bool) -> Layers {
        let listed = !self.menu().is_empty();
        self.layers_with(running, listed)
    }

    // `layers`, for a caller that already knows whether a list is up.
    pub(super) fn layers_with(&self, running: bool, listed: bool) -> Layers {
        Layers {
            surface: match listed {
                true => Surface::Completion,
                false => Surface::Editor,
            },
            run: running,
            mode: self.vim.unwrap_or_default(),
            // Whether the line has anything on it, which the keys bound
            // to an empty one read.
            line_empty: self.editor.is_empty(),
            // A search reads the transcript as a page is read: `n` steps,
            // esc lets go.
            page: self.reading(),
        }
    }

    // Until when the character the last press typed may still be taken
    // back by the press after it, if it may.
    pub(super) fn held(&self) -> Option<Instant> {
        let last = self.last_press.as_ref().filter(|l| l.typed && l.opens)?;
        let until = last.at + self.pair_window_after(last.press);
        (Instant::now() < until).then_some(until)
    }

    // How long `press` waits for the second of a pair. A bare character is
    // also text, so its pairs must come quickly or typing would trip them.
    fn pair_window_after(&self, press: Press) -> std::time::Duration {
        if press.is_bare() {
            self.pair_window
        } else {
            DOUBLE_TAP
        }
    }

    // The stop: whatever is over the editor closes, and a run stops. With
    // neither, it says the next one leaves.
    fn cancel(&mut self, view: &mut View, running: bool) -> Asked {
        let focused = self.background_focus.is_some();
        self.leave_background();
        let closed = self.close_overlay(view) || focused;
        if running {
            return Asked::Own(Deed::Interrupt);
        }
        if closed {
            return Asked::Own(Deed::Nothing);
        }
        self.flash("press it again to quit");
        Asked::Own(Deed::Nothing)
    }

    // The page over the transcript closed, or the search on it let go;
    // false when neither was up. The press that closed it is not the first
    // of a pair.
    fn close_overlay(&mut self, view: &mut View) -> bool {
        if !self.close_page() && !self.search_close(view) {
            return false;
        }
        self.disarm();
        true
    }

    // A press that did its own thing is not the first of a pair: the table
    // pairs by key alone (dismissing `esc`, clearing `ctrl+l`).
    pub(super) fn disarm(&mut self) {
        self.last_press = None;
    }

    // The line has the keys, a completion list perhaps over it.
    fn editor_key(
        &mut self,
        lane: &Lane,
        view: &mut View,
        key: crossterm::event::KeyEvent,
        action: Option<Action>,
    ) -> Asked {
        if let Some(action) = action
            && (self.editor.edit(action) || self.vim_edit(action) || self.move_view(view, action))
        {
            return Asked::Own(Deed::Nothing);
        }
        match action {
            Some(Action::LineSubmit) => return self.enter(view),
            Some(Action::EditExternally) => return Asked::Own(Deed::External),
            Some(Action::PasteImage) => self.paste_image(),
            Some(Action::RunInterrupt) => return self.interrupt(lane, view),
            Some(Action::Rewind) => return Asked::Own(Deed::Rewind),
            Some(action @ (Action::LaneNext | Action::LanePrev)) => {
                return self.step_lane(lane, action == Action::LaneNext);
            }
            // The session is what a press would replace, so it takes two.
            Some(Action::SessionNew) => return Asked::Core(Intent::Builtin(Builtin::New)),
            // No `running` check: leaving is one intent whatever is in
            // flight, and `admit` gives it one answer.
            Some(Action::AppExit) => return Asked::Core(Intent::Builtin(Builtin::Quit)),
            // What only the screen holds goes before the line: a notice
            // landing mid-sentence must not cost the sentence.
            Some(Action::LineClear)
                if view
                    .surface
                    .scrollback
                    .iter()
                    .any(super::row::Row::is_aside) =>
            {
                self.disarm();
                return Asked::Own(Deed::Tidy);
            }
            Some(Action::LineClear) => {
                if !self.editor.is_empty() {
                    self.editor.clear();
                    self.disarm();
                }
            }
            Some(Action::HistoryOlder) => {
                let cancelled = self.history_older(view);
                if !cancelled.is_empty() {
                    return Asked::Own(Deed::RecallQueued(cancelled));
                }
            }
            Some(Action::HistoryNewer) => self.editor.down(),
            Some(Action::MenuAccept) => self.menu_accept(view),
            Some(action @ (Action::MenuNext | Action::MenuPrevious)) => self.step_menu(action),
            // Only a question's options are sent together.
            Some(Action::MenuMark) => {
                if let Some(MenuEntry::Pick { at, .. }) = self.highlighted() {
                    self.list.mark(at);
                }
            }
            // A question's list is let go only by closing the question.
            Some(Action::MenuDismiss)
                if matches!(self.highlighted(), Some(MenuEntry::Pick { .. })) =>
            {
                return self.answer(&[], None);
            }
            Some(Action::MenuDismiss) => self.menu_dismiss(),
            None => self.type_key(&key),
            // Another focus's, or a guard above that did not hold (`esc esc`
            // with text on the line): nothing here.
            Some(_) => {}
        }
        Asked::Own(Deed::Nothing)
    }

    // Enter: the highlighted completion first, since it may continue the
    // input rather than submit it; else the line goes.
    fn enter(&mut self, view: &mut View) -> Asked {
        self.list.seeded = None;
        // What is sent is answered on the transcript, not the page.
        self.close_overlay(view);
        let moved = self.list.picked.is_some();
        match self.highlighted().filter(|e| e.taken_by_enter(moved)) {
            // What was marked, or with nothing marked the highlighted one.
            Some(MenuEntry::Pick { at, .. }) => {
                let picks: Vec<usize> = match self.list.marked.is_empty() {
                    true => vec![at],
                    false => self.list.marked.iter().copied().collect(),
                };
                self.answer(&picks, None)
            }
            Some(MenuEntry::Completion(c)) => match c.accept {
                Acceptance::Continue => {
                    self.apply_completion(c);
                    Asked::Own(Deed::Nothing)
                }
                Acceptance::Run => {
                    self.editor.take();
                    self.run_line(c.line)
                }
            },
            Some(MenuEntry::File {
                start,
                end,
                lead,
                path,
                dir,
                ..
            }) => {
                // Enter on a path applies it and stays: the prompt
                // is not done until the user says so.
                self.apply_file(start, end, lead, &path, dir);
                Asked::Own(Deed::Nothing)
            }
            Some(MenuEntry::Hit { .. }) | None => {
                let typed = self.editor.take();
                // Words matching none of a question's options answer it.
                let own =
                    !self.questions.is_empty() && !typed.trim().is_empty() && input::plain(&typed);
                match own {
                    true => self.answer(&[], Some(typed)),
                    false => self.run_line(typed),
                }
            }
        }
    }

    // The clipboard's image, saved and named on the line; an image already
    // pasted keeps its number.
    fn paste_image(&mut self) {
        match super::clipboard::paste_image() {
            Ok((path, about)) => {
                let n = match self.images.iter().position(|p| *p == path) {
                    Some(at) => at + 1,
                    None => {
                        self.images.push(path);
                        self.images.len()
                    }
                };
                self.editor.insert_str(&format!("[Image #{n} {about}] "));
            }
            Err(why) => self.flash(why),
        }
    }

    // Esc before the model has moved means "I didn't mean to send that"; an
    // empty editor, or unsending overwrites a line.
    fn interrupt(&self, lane: &Lane, view: &View) -> Asked {
        let unsendable = view
            .state
            .work
            .as_ref()
            .is_some_and(|w| matches!(w.doing, Doing::Turn { committed: false }));
        if self.editor.is_empty() && lane.is_running() && unsendable {
            return Asked::Own(Deed::Unsend);
        }
        Asked::Own(Deed::Interrupt)
    }

    fn step_lane(&mut self, lane: &Lane, forward: bool) -> Asked {
        match self.step_checkout(lane, forward) {
            Some(name) => Asked::Core(Intent::Builtin(Builtin::Worktree(name))),
            None => {
                self.flash("the only checkout there is");
                Asked::Own(Deed::Nothing)
            }
        }
    }

    // The vim edits that leave Normal or change the line; false for any
    // other action.
    fn vim_edit(&mut self, action: Action) -> bool {
        match action {
            Action::InsertNewline => self.editor.insert('\n'),
            Action::ModeNormal => self.set_mode(Mode::Normal),
            Action::ModeInsert => self.leave_normal(),
            Action::ModeInsertAfter => {
                self.editor.right();
                self.leave_normal();
            }
            Action::ModeInsertLineStart => {
                self.editor.home();
                self.leave_normal();
            }
            Action::ModeInsertLineEnd => {
                self.editor.end();
                self.leave_normal();
            }
            Action::ChangeChar => {
                self.editor.delete();
                self.leave_normal();
            }
            Action::ChangeToLineEnd => {
                self.editor.kill_to_end();
                self.leave_normal();
            }
            Action::ChangeLine => self.change_line(),
            Action::OpenLineBelow => {
                self.editor.open_below();
                self.leave_normal();
            }
            Action::OpenLineAbove => {
                self.editor.open_above();
                self.leave_normal();
            }
            _ => return false,
        }
        true
    }

    // What moves or folds the view rather than the line; false for any
    // other action.
    fn move_view(&mut self, view: &mut View, action: Action) -> bool {
        match action {
            Action::MoveBufferStart => self.buffer_ends(view, true),
            Action::MoveBufferEnd => self.buffer_ends(view, false),
            Action::ScrollPageUp => self.scroll_view(view, true, self.page_scroll_step()),
            Action::ScrollPageDown => self.scroll_view(view, false, self.page_scroll_step()),
            Action::ScrollHalfUp => self.scroll_view(view, true, self.half_scroll_step()),
            Action::ScrollHalfDown => self.scroll_view(view, false, self.half_scroll_step()),
            Action::ScrollLineDown => self.scroll_view(view, false, 1),
            Action::ScrollLineUp => self.scroll_view(view, true, 1),
            Action::PageClose => {
                self.close_overlay(view);
            }
            Action::PageSearchNext | Action::PageSearchPrev => {
                self.search_step(action == Action::PageSearchNext);
            }
            Action::Search => self.search_open(view),
            // The last group only. The switch is left alone, so the
            // groups no one is touching keep what they had.
            Action::ThinkFold => view
                .surface
                .folds
                .toggle_current(&mut view.surface.scrollback),
            // Every group in the scrollback, last one included, and
            // the switch with them: one key resets the whole screen.
            Action::ThinkFoldAll => view.surface.folds.flip_all(&mut view.surface.scrollback),
            _ => return false,
        }
        true
    }

    fn menu_accept(&mut self, view: &mut View) {
        match self.highlighted() {
            Some(MenuEntry::Completion(c)) => self.apply_completion(c),
            Some(MenuEntry::File {
                start,
                end,
                lead,
                path,
                dir,
                ..
            }) => self.apply_file(start, end, lead, &path, dir),
            Some(MenuEntry::Hit { .. }) => self.search_enter(view),
            // Onto the line, to send as it is or changed.
            Some(MenuEntry::Pick { at, .. }) => {
                let option = self.questions.first().and_then(|q| q.options.get(at));
                if let Some(option) = option.cloned() {
                    self.editor.set_line(&option);
                }
            }
            None => {}
        }
    }

    fn menu_dismiss(&mut self) {
        if self.list.seeded.take().as_deref() == Some(self.editor.text()) {
            self.editor.clear();
        }
        // Keyed to the text, so any edit brings the list back —
        // "not that", not "never again".
        self.list.dismissed_at = Some(self.editor.text().to_string());
        self.disarm();
    }

    // Unbound and printable is the one thing no table has to say — except
    // in Normal, where it is the table saying no.
    fn type_key(&mut self, key: &crossterm::event::KeyEvent) {
        if let Some(c) = bare_letter(key)
            && self.vim.is_none_or(|m| m == Mode::Insert)
        {
            self.editor.insert(c);
            if let Some(last) = &mut self.last_press {
                last.typed = true;
            }
        }
    }

    // The wheel scrolls what is shown: the reading page while one is up,
    // else the transcript.
    fn mouse(&mut self, view: &mut View, mouse: crossterm::event::MouseEvent) -> Asked {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.selection = None;
                self.scroll_view(view, true, 1);
            }
            MouseEventKind::ScrollDown => {
                self.selection = None;
                self.scroll_view(view, false, 1);
            }
            MouseEventKind::Moved => self.on_mouse_move(view, mouse.column, mouse.row),
            MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                self.on_mouse_move(view, mouse.column, mouse.row);
                self.on_drag(mouse.column, mouse.row);
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                if let Some(id) = self.page_command_at(mouse.column, mouse.row) {
                    self.press = None;
                    self.selection = None;
                    return Asked::Own(Deed::ReadCommand(id));
                }
                if !self.reading() {
                    let deed = match self.target_at(mouse.row) {
                        Some(super::mouse::Target::BackgroundResults(_)) => {
                            Some(Deed::BackgroundResults(!self.background_results_shown))
                        }
                        _ => None,
                    };
                    if let Some(deed) = deed {
                        self.press = None;
                        self.selection = None;
                        return Asked::Own(deed);
                    }
                }
                self.on_press(view, mouse.column, mouse.row);
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                if let Some(id) = self.on_release(view, mouse.column, mouse.row) {
                    return Asked::Own(Deed::ToggleCommand(id));
                }
            }
            _ => {}
        }
        Asked::Own(Deed::Nothing)
    }

    // Up from the bottom of recall takes back the typed lines still queued,
    // oldest first and the draft after them, before reaching history.
    fn history_older(&mut self, view: &mut View) -> Vec<pi_core::execution::Request> {
        let mut queued = view.queued.lock();
        let mut cancelled = Vec::new();
        if self.editor.recalls_next() && queued.iter().any(|q| q.line.is_some()) {
            let mut lines = Vec::new();
            queued.retain_mut(|q| match q.line.take() {
                Some(line) => {
                    lines.push(line);
                    cancelled.push(q.request);
                    false
                }
                None => true,
            });
            if !self.editor.is_empty() {
                lines.push(self.editor.text().to_string());
            }
            self.editor.set_line(&lines.join("\n"));
            return cancelled;
        }
        self.editor.up();
        cancelled
    }

    // Submit `line` as typed: kept in history if it can be recalled, then read.
    fn run_line(&mut self, line: String) -> Asked {
        if input::recallable(&line, &self.commands) {
            self.editor.remember(&line);
        }
        if line.trim().is_empty() {
            return Asked::Own(Deed::Nothing);
        }
        let intent = input::read(
            &line,
            &self.commands,
            self.tools.as_ref().map(|brief| &brief.registry),
        );
        // Bare, it asks which session: the line's completion of it lists them.
        if matches!(&intent, Intent::Builtin(Builtin::Resume(id)) if id.is_empty()) {
            self.seed(super::RESUME);
            if self.menu().is_empty() {
                self.editor.clear();
                self.list.seeded = None;
                self.flash(super::NO_SESSIONS);
            }
            return Asked::Own(Deed::Nothing);
        }
        self.submitted = Some(line);
        Asked::Core(intent)
    }
}

/// The press a terminal key is, as the key table reads it. No catch-all: a key
/// a new crossterm adds does not compile until it is given a name or `Other`.
pub(super) fn press_of(key: &KeyEvent) -> Press {
    let code = match key.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::F(n) => Key::F(n),
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Insert => Key::Insert,
        KeyCode::Esc => Key::Esc,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Null
        | KeyCode::CapsLock
        | KeyCode::ScrollLock
        | KeyCode::NumLock
        | KeyCode::PrintScreen
        | KeyCode::Pause
        | KeyCode::Menu
        | KeyCode::KeypadBegin
        | KeyCode::Media(_)
        | KeyCode::Modifier(_) => Key::Other,
    };
    let m = key.modifiers;
    let mods = Mods {
        ctrl: m.contains(KeyModifiers::CONTROL),
        alt: m.contains(KeyModifiers::ALT),
        shift: m.contains(KeyModifiers::SHIFT),
    };
    Press::of(code, mods)
}

/// The character a key types, when it types one on its own: a letter with
/// `ctrl` or `alt` on it is the menu's key rather than the surface's own, which
/// is the same rule every screen with a letter vocabulary reads — a page, a
/// search — so it is written down once, here.
pub(super) fn bare_letter(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c) if press_of(key).is_bare() => Some(c),
        _ => None,
    }
}
