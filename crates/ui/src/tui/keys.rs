//! Where a press goes: the key table read against who has the keyboard —
//! the line, the rewind menu or the reading page — and the mouse.
use super::browse::Page;
use super::menu::MenuEntry;
use super::ui::{Focus, LastPress};
use super::view::View;
use super::{Asked, Deed, Ui};
use super::{DOUBLE_TAP, screen};
use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use pi_core::core::lane::Lane;
use pi_core::input::Builtin;
use pi_core::input::{self, Intent};
use pi_store::keys::{Action, Key, Layers, Mode, Mods, Press, Surface};
use std::time::Instant;

impl Ui {
    // The line submitted since this was last asked, if any: the surface
    // catches the recall list up and carries the line to its answer.
    pub(super) fn took_submit(&mut self) -> Option<String> {
        std::mem::take(&mut self.submitted)
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
                self.last_press = None;
                // Only the line or a query being typed takes text: anywhere
                // else the paste would land where nobody can see it.
                if matches!(self.focus, Focus::Editor) {
                    self.editor.insert_str(&text.replace('\r', "\n"));
                } else {
                    self.search_paste(&text);
                }
                return Asked::Own(Deed::Nothing);
            }
            TermEvent::Mouse(mouse) => return self.mouse(view, mouse),
            // Windows reports both press and release; acting on both would
            // double every keystroke.
            TermEvent::Key(k) if k.kind != KeyEventKind::Release => {
                self.selection = None;
                k
            }
            _ => return Asked::Own(Deed::Nothing),
        };
        if self.search_key(&key) {
            return Asked::Own(Deed::Nothing);
        }

        let press = press_of(&key);
        let now = Instant::now();
        let last = self.last_press.take();
        let prev = last
            .filter(|l| now.duration_since(l.at) < self.pair_window_after(l.press))
            .map(|l| l.press);
        let mut layers = self.layers(running);
        let mut hit = self.keys.hit(prev, press, layers);
        // Output with nothing to page through is gone at the next key, which
        // goes to the line; only the page's own close keys stop there.
        let stays = |a| matches!(a, Action::PagerClose | Action::AppCancel | Action::AppQuit);
        if self.slash_fits() && !hit.is_some_and(|h| stays(h.action)) {
            self.close_overlay();
            layers = self.layers(running);
            hit = self.keys.hit(None, press, layers);
        }
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
            });
        }
        let action = hit.map(|h| h.action);

        match action {
            Some(Action::AppCancel) => return self.cancel(running),
            Some(Action::AppQuit) => return Asked::Core(Intent::Builtin(Builtin::Quit)),
            _ => {}
        }
        match self.focus {
            Focus::Read(_) => self.pager_key(view, action),
            Focus::Rewind(_) => self.rewind_key(action),
            Focus::Editor => self.editor_key(lane, view, key, action),
        }
    }

    // A command's output that fits its region whole.
    pub(super) fn slash_fits(&self) -> bool {
        matches!(self.page(), Some(Page::Slash(lines))
            if lines.len() <= self.regions.history.height as usize)
    }

    // The layers the key table reads, from who has the keyboard.
    fn layers(&mut self, running: bool) -> Layers {
        let surface = match self.focus {
            Focus::Read(_) => Surface::Pager,
            Focus::Rewind(_) => Surface::Picker,
            Focus::Editor => {
                if self.menu().is_empty() {
                    Surface::Editor
                } else {
                    Surface::Completion
                }
            }
        };
        Layers {
            surface,
            run: running,
            mode: self.vim,
            // Whether the line has anything on it, which the keys bound
            // to an empty one read. Only Normal asks.
            line_empty: self.editor.is_empty(),
        }
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
    fn cancel(&mut self, running: bool) -> Asked {
        let closed = self.close_overlay();
        if running {
            return Asked::Own(Deed::Interrupt);
        }
        if closed {
            return Asked::Own(Deed::Nothing);
        }
        self.flash("press it again to quit");
        Asked::Own(Deed::Nothing)
    }

    // Back to the line, from whatever has the keyboard; false when the line
    // had it already. The press that closed is not the first of a pair.
    fn close_overlay(&mut self) -> bool {
        match self.focus {
            Focus::Editor => return false,
            Focus::Read(_) | Focus::Rewind(_) => self.focus = Focus::Editor,
        }
        self.disarm();
        true
    }

    // A press that did its own thing is not the first of a pair: the table
    // pairs by key alone (dismissing `esc`, clearing `ctrl+l`).
    fn disarm(&mut self) {
        self.last_press = None;
    }

    // The reading page: the pager keys move it, and everything else is
    // swallowed.
    fn pager_key(&mut self, view: &mut View, action: Option<Action>) -> Asked {
        let (up, step) = match action {
            Some(Action::PagerDown) => (false, Stride::Line),
            Some(Action::PagerUp) => (true, Stride::Line),
            Some(Action::PagerHalfDown) => (false, Stride::Half),
            Some(Action::PagerHalfUp) => (true, Stride::Half),
            Some(Action::PagerPageDown) => (false, Stride::Page),
            Some(Action::PagerPageUp) => (true, Stride::Page),
            Some(Action::PagerBottom) => (false, Stride::End),
            Some(Action::PagerTop) => (true, Stride::End),
            Some(Action::PagerClose) => {
                self.close_overlay();
                return Asked::Own(Deed::Nothing);
            }
            Some(Action::PagerSearch) => {
                self.search_open();
                return Asked::Own(Deed::Nothing);
            }
            Some(action @ (Action::PagerSearchNext | Action::PagerSearchPrev)) => {
                self.search_step(action == Action::PagerSearchNext);
                return Asked::Own(Deed::Nothing);
            }
            _ => return Asked::Own(Deed::Nothing),
        };
        let rows = match step {
            Stride::Line => 1,
            Stride::Half => self.half_scroll_step(),
            Stride::Page => self.page_scroll_step(),
            Stride::End => screen::TOP,
        };
        self.scroll_view(view, up, rows);
        Asked::Own(Deed::Nothing)
    }

    // The rewind selector: move, pick, or close; nothing else reaches it.
    fn rewind_key(&mut self, action: Option<Action>) -> Asked {
        match action {
            Some(action @ (Action::MenuNext | Action::MenuPrevious)) => self.step_menu(action),
            Some(Action::MenuFirst) => self.picked = Some(0),
            Some(Action::MenuLast) => self.picked = Some(self.menu().len().saturating_sub(1)),
            Some(Action::MenuAccept) => {
                if let Some(MenuEntry::Message { id, .. }) = self.highlighted() {
                    self.focus = Focus::Editor;
                    return Asked::Own(Deed::To(id));
                }
            }
            Some(Action::MenuDismiss) => {
                self.focus = Focus::Editor;
                self.disarm();
            }
            _ => {}
        }
        Asked::Own(Deed::Nothing)
    }

    // The line has the keys, a completion list perhaps over it.
    fn editor_key(
        &mut self,
        lane: &Lane,
        view: &mut View,
        key: crossterm::event::KeyEvent,
        action: Option<Action>,
    ) -> Asked {
        match action {
            Some(Action::LineSubmit) => {
                // Enter with a menu open runs what's highlighted, the typed
                // text a mere prefix. Pick before draining the editor.
                return match self.highlighted() {
                    Some(MenuEntry::Completion(c)) if !c.picked_only || self.picked.is_some() => {
                        // The completion's line runs; the prefix that
                        // produced it goes, so it can't resubmit as a stray prompt.
                        self.editor.take();
                        self.run_line(c.line)
                    }
                    Some(MenuEntry::File {
                        start,
                        end,
                        path,
                        dir,
                        ..
                    }) => {
                        // Enter on a path applies it and stays: the prompt
                        // is not done until the user says so.
                        self.apply_file(start, end, &path, dir);
                        Asked::Own(Deed::Nothing)
                    }
                    Some(MenuEntry::Completion(_) | MenuEntry::Message { .. }) | None => {
                        let typed = self.editor.take();
                        self.run_line(typed)
                    }
                };
            }
            Some(Action::EditExternally) => return Asked::Own(Deed::External),
            Some(Action::PasteImage) => match super::clipboard::paste_image() {
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
            },
            Some(Action::RunInterrupt) => {
                // Esc before the model has moved means "I didn't mean to send
                // that"; an empty editor, or unsending overwrites a line.
                if self.editor.is_empty() && lane.is_running() && !view.state.committed {
                    return Asked::Own(Deed::Unsend);
                }
                return Asked::Own(Deed::Interrupt);
            }
            // Only from an empty line: with text on it, `esc esc` is nothing.
            Some(Action::Rewind) if self.editor.is_empty() => {
                return Asked::Own(Deed::Rewind);
            }
            Some(action @ (Action::LaneNext | Action::LanePrev)) => {
                let forward = action == Action::LaneNext;
                return match self.step_checkout(lane, forward) {
                    Some(name) => Asked::Core(Intent::Builtin(Builtin::Worktree(name))),
                    None => {
                        self.flash("the only checkout there is");
                        Asked::Own(Deed::Nothing)
                    }
                };
            }
            // The session is what a press would replace, so it takes two,
            // and only with nothing on the line.
            Some(Action::SessionNew) if self.editor.is_empty() => {
                return Asked::Core(Intent::Builtin(Builtin::New));
            }
            Some(Action::AppExit) => {
                // No `running` check: leaving is one intent whatever is in
                // flight, and `admit` gives it one answer.
                return if self.editor.is_empty() {
                    Asked::Core(Intent::Builtin(Builtin::Quit))
                } else {
                    self.editor.delete();
                    Asked::Own(Deed::Nothing)
                };
            }
            Some(Action::LineClear) => {
                if !self.editor.is_empty() {
                    self.editor.clear();
                    self.disarm();
                }
            }
            Some(Action::InsertNewline) => self.editor.insert('\n'),
            Some(Action::DeleteCharBack) => self.editor.backspace_tag(),
            Some(Action::DeleteCharForward) => self.editor.delete_tag(),
            Some(Action::DeleteWordBack) => self.editor.kill_word_back(),
            Some(Action::DeleteToLineEnd) => self.editor.kill_to_end(),
            Some(Action::DeleteToLineStart) => self.editor.kill_to_start(),
            Some(Action::DeleteLine) => self.editor.delete_line(),
            Some(Action::MoveCharLeft) => self.editor.left(),
            Some(Action::MoveCharRight) => self.editor.right(),
            Some(Action::MoveWordLeft) => self.editor.word_left(),
            Some(Action::MoveWordRight) => self.editor.word_right(),
            Some(Action::MoveWordNext) => self.editor.word_next(),
            Some(Action::ModeNormal) => self.set_mode(Mode::Normal),
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
            Some(Action::ChangeLine) => self.change_line(),
            Some(Action::OpenLineBelow) => {
                self.editor.open_below();
                self.leave_normal();
            }
            Some(Action::OpenLineAbove) => {
                self.editor.open_above();
                self.leave_normal();
            }
            Some(Action::MoveLineStart) => self.editor.home(),
            Some(Action::MoveLineEnd) => self.editor.end(),
            Some(Action::MoveLineFirstNonBlank) => self.editor.first_non_blank(),
            Some(Action::MoveBufferStart) => self.buffer_ends(view, true),
            Some(Action::MoveBufferEnd) => self.buffer_ends(view, false),
            Some(Action::HistoryOlder) => self.history_older(view),
            Some(Action::HistoryNewer) => self.editor.down(),
            Some(Action::ScrollPageUp) => self.scroll_view(view, true, self.page_scroll_step()),
            Some(Action::ScrollPageDown) => self.scroll_view(view, false, self.page_scroll_step()),
            Some(Action::ScrollHalfUp) => self.scroll_view(view, true, self.half_scroll_step()),
            Some(Action::ScrollHalfDown) => self.scroll_view(view, false, self.half_scroll_step()),
            Some(Action::Browse) => self.browse(),
            Some(Action::BrowseSearch) => self.search_conversation(),
            Some(Action::ThinkFold) => {
                // The last group only. The switch is left alone, so the
                // groups no one is touching keep what they had.
                view.surface
                    .folds
                    .toggle_current(&mut view.surface.scrollback);
            }
            Some(Action::ThinkFoldAll) => {
                // Every group in the scrollback, last one included, and
                // the switch with them: one key resets the whole screen.
                view.surface.folds.flip_all(&mut view.surface.scrollback);
            }
            Some(Action::MenuAccept) => match self.highlighted() {
                Some(MenuEntry::Completion(c)) => {
                    self.editor.set_line(&c.line);
                    // Something still expected after it wants a space first.
                    if c.more {
                        self.editor.insert(' ');
                    }
                }
                Some(MenuEntry::File {
                    start,
                    end,
                    path,
                    dir,
                    ..
                }) => self.apply_file(start, end, &path, dir),
                Some(MenuEntry::Message { .. }) | None => {}
            },
            Some(action @ (Action::MenuNext | Action::MenuPrevious)) => self.step_menu(action),
            Some(Action::MenuDismiss) => {
                // Keyed to the text, so any edit brings the list back —
                // "not that", not "never again".
                self.dismissed_at = Some(self.editor.text().to_string());
                self.disarm();
            }
            // Unbound and printable is the one thing no table has to say —
            // except in Normal, where it is the table saying no.
            None => {
                if let Some(c) = bare_letter(&key)
                    && self.vim.is_none_or(|m| m == Mode::Insert)
                {
                    self.editor.insert(c);
                    if let Some(last) = &mut self.last_press {
                        last.typed = true;
                    }
                }
            }
            // Another focus's, or a guard above that did not hold (`esc esc`
            // with text on the line): nothing here.
            Some(_) => {}
        }
        Asked::Own(Deed::Nothing)
    }

    // The wheel scrolls what has the keyboard: the reading page while one
    // is up, else the transcript.
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
                self.on_press(view, mouse.column, mouse.row);
            }
            MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                if let Some(id) = self.on_release(view, mouse.column, mouse.row) {
                    return Asked::Own(Deed::Job(id));
                }
            }
            _ => {}
        }
        Asked::Own(Deed::Nothing)
    }

    // Up from the bottom of recall takes back the typed lines still queued,
    // oldest first and the draft after them, before reaching history.
    fn history_older(&mut self, view: &mut View) {
        if self.editor.recalls_next() && view.queued.iter().any(|q| q.line.is_some()) {
            let mut lines = Vec::new();
            view.queued.retain_mut(|q| match q.line.take() {
                Some(line) => {
                    lines.push(line);
                    false
                }
                None => true,
            });
            if !self.editor.is_empty() {
                lines.push(self.editor.text().to_string());
            }
            self.editor.set_line(&lines.join("\n"));
            return;
        }
        self.editor.up();
    }

    // Submit `line` as typed: kept in history if it can be recalled, then read.
    fn run_line(&mut self, line: String) -> Asked {
        if input::recallable(&line, &self.commands) {
            self.editor.remember(&line);
        }
        if line.trim().is_empty() {
            return Asked::Own(Deed::Nothing);
        }
        let intent = input::read(&line, &self.commands);
        self.submitted = Some(line);
        Asked::Core(intent)
    }
}

// How far a pager key moves what it reads.
enum Stride {
    Line,
    Half,
    Page,
    // As far as there is.
    End,
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
/// is the same rule every screen with a letter vocabulary reads — browse, a
/// slash page, a preview's search — so it is written down once, here.
pub(super) fn bare_letter(key: &KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c) if press_of(key).is_bare() => Some(c),
        _ => None,
    }
}
