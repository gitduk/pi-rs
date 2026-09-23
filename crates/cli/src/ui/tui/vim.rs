//! The modal keys: what a press means when the keys are not just text, and
//! where the modes are kept.

use std::time::Instant;

use super::view::View;
use super::{DOUBLE_TAP, Ui, screen};
use crate::store::keys::{Action, Mode};

impl Ui {
    // Back to Insert. The half-typed escape character goes with the mode: it
    // belonged to a line nobody is commanding any more.
    pub(super) fn leave_normal(&mut self) {
        if let Some(v) = &mut self.vim {
            v.mode = Mode::Insert;
            v.last = None;
        }
        self.show_mode();
    }

    // The line, rewritten from nothing: `S`, or `cc` by its doubled spelling.
    // Both leave, because what follows is typing.
    pub(super) fn change_line(&mut self) {
        self.editor.clear_line();
        self.leave_normal();
    }

    // `gg` and `G` command the line's buffer while there is a line to command;
    // with the line empty they command the history's ends instead.
    pub(super) fn buffer_ends(&mut self, view: &mut View, start: bool) {
        if self.editor.is_empty() {
            // One step past either end, which the window clamps back to it.
            self.scroll_view(view, start, screen::TOP);
        } else if start {
            self.editor.buffer_start();
        } else {
            self.editor.buffer_end();
        }
    }

    // Put the mode where it can be seen: the shape of the caret, and the
    // the prompt sigil where the theme gives the two modes different ones. It
    // does not by default — one bar either way — because the caret is where
    // the eye already is; a terminal that will not reshape it is what
    // `prompt.normal` is for.
    //
    // This is what pays for the mode never resetting itself. A mode that
    // persists across submitted lines and cannot be seen would be a trap;
    // one that can be seen is just where you left it.
    pub(super) fn show_mode(&mut self) {
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

    // Follow what the config says about the modal keys.
    //
    // Turning them off drops the state rather than parking it: coming back
    // later in Normal, with no keystroke between having asked to go there,
    // is the one surprise this has to rule out. Turning them off is also the
    // only thing that changes the mode without a key — everything else keeps
    // whichever mode was last asked for, submitted lines included.
    pub(super) fn set_vim(&mut self, cfg: &crate::store::config::Vim) {
        match (&mut self.vim, cfg.enabled) {
            (slot @ None, true) => *slot = Some(Vim::new(cfg)),
            (slot, false) => *slot = None,
            (Some(v), true) => v.configure(cfg),
        }
        self.show_mode();
    }
}

// Whether a press lands inside the double-tap window of the previous one,
// and records the press either way.
pub(super) fn double_tap(last: &mut Option<Instant>, now: Instant) -> bool {
    let hit = last.is_some_and(|p| now.duration_since(p) < DOUBLE_TAP);
    *last = Some(now);
    hit
}

// What a typed character means to the modal keys.
pub(super) enum Typed {
    // It lands in the line, as it would with vim keys off.
    Insert,
    // It closed the escape sequence: the half already on screen has to come
    // back off, and the mode has changed.
    Escape,
    // Normal mode. An unbound character commands nothing and types nothing —
    // without this the mode would be a costume, every key still typing.
    Ignore,
    // Normal mode finished a doubled key (`dd`, `gg`, `cc`): the action it
    // names, for the caller to answer.
    Command(Action),
}

// The modal keys' whole state: the mode that is up, the sequence that leaves
// Insert, and the character that may be its first half.
//
// One struct rather than four fields on `Ui`: none of them means anything
// without the others, and `Ui` already carries more loose state than it
// should. `Ui` holds it as an `Option`, so vim being off is the absence of
// the state rather than a flag beside it — "off, but in Normal" cannot be
// written down.
pub(super) struct Vim {
    pub(super) mode: Mode,
    // The two characters that leave Insert, resolved once. `None` — an empty
    // setting, or any other length — is no sequence, and with it no way into
    // Normal at all.
    pub(super) escape: Option<(char, char)>,
    pub(super) window: std::time::Duration,
    // The last character typed, and when. Lazy, like the double-taps: the
    // character is on screen already and nothing is held pending, so the line
    // is never a guess about a key that has not arrived.
    pub(super) last: Option<(char, Instant)>,
}

impl Vim {
    pub(super) fn new(cfg: &crate::store::config::Vim) -> Self {
        let mut vim = Self {
            mode: Mode::Insert,
            escape: None,
            window: std::time::Duration::ZERO,
            last: None,
        };
        vim.configure(cfg);
        vim
    }

    // Take what the config says about the sequence, resolving the two
    // characters here rather than at every keystroke. Anything that is not
    // exactly two of them is no sequence — the documented way to leave
    // Normal unreachable while keeping the layer's bindings listed.
    pub(super) fn configure(&mut self, cfg: &crate::store::config::Vim) {
        self.escape = cfg.escape_pair();
        self.window = std::time::Duration::from_millis(cfg.escape_timeout_ms);
    }

    // The doubled keys — `dd`, `gg`, `cc` — are the escape pair's
    // Normal-mode cousins: the same character twice inside the same window.
    pub(super) fn doubled(c: char) -> Option<Action> {
        match c {
            'd' => Some(Action::DeleteLine),
            'g' => Some(Action::MoveBufferStart),
            'c' => Some(Action::ChangeLine),
            _ => None,
        }
    }

    // Was `prev` the character typed just now? The take spends the stored
    // half either way.
    pub(super) fn armed(&mut self, prev: char, now: Instant) -> bool {
        self.last
            .take()
            .is_some_and(|(p, at)| p == prev && now.duration_since(at) < self.window)
    }

    // Was this press the second half of a doubled key? One that was not is the
    // half the next one needs, which is what makes `dd` a pair and `d` nothing.
    pub(super) fn completes(&mut self, c: char, now: Instant) -> bool {
        if self.armed(c, now) {
            return true;
        }
        self.last = Some((c, now));
        false
    }

    // What `c` does, and the mode change if it makes one.
    pub(super) fn typed(&mut self, c: char, now: Instant) -> Typed {
        if self.mode == Mode::Normal {
            if let Some(action) = Self::doubled(c) {
                if self.completes(c, now) {
                    return Typed::Command(action);
                }
            } else {
                self.last = None;
            }
            return Typed::Ignore;
        }
        let Some((first, second)) = self.escape else {
            return Typed::Insert;
        };
        if self.armed(first, now) && c == second {
            self.mode = Mode::Normal;
            return Typed::Escape;
        }
        self.last = Some((c, now));
        Typed::Insert
    }
}
