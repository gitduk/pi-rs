//! The modal keys: what a press means when the keys are not just text, and
//! where the modes are kept.

use std::time::Instant;

use super::view::View;
use super::{DOUBLE_TAP, Ui, screen};
use pi_store::keys::{Action, Mode};

impl Ui {
    // Back to Insert; drop any half-typed escape char, it belonged to that mode.
    pub(super) fn leave_normal(&mut self) {
        if let Some(v) = &mut self.vim {
            v.mode = Mode::Insert;
            v.last = None;
        }
        self.show_mode();
    }

    // `S`/`cc`: clear the line and drop back to Insert, since typing follows.
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

    // Shows the mode via caret shape (and prompt icon, if themed) so a mode
    // that persists across lines is visible, not a silent trap.
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

    // Turning modal keys off drops the state (not parks it), so coming back
    // never resumes Normal without an intervening keystroke.
    pub(super) fn set_vim(&mut self, cfg: &pi_store::config::Vim) {
        match (&mut self.vim, cfg.enabled) {
            (slot @ None, true) => *slot = Some(Vim::new(cfg)),
            (slot, false) => *slot = None,
            (Some(v), true) => v.configure(cfg),
        }
        self.show_mode();
    }
}

// True if within the previous window; always records this press too.
pub(super) fn double_tap(last: &mut Option<Instant>, now: Instant) -> bool {
    let hit = last.is_some_and(|p| now.duration_since(p) < DOUBLE_TAP);
    *last = Some(now);
    hit
}

// What a typed character means to the modal keys.
pub(super) enum Typed {
    // It lands in the line, as it would with vim keys off.
    Insert,
    // Closes the escape sequence: the half-typed char comes back off,
    // mode changed.
    Escape,
    // Normal mode, unbound key: commands and types nothing — without this
    // Normal would be a costume, since every key would still type.
    Ignore,
    // Normal mode, doubled key (`dd`, `gg`, `cc`) completed: the action for
    // the caller to carry out.
    Command(Action),
}

// Modal-key state, held as one `Option` on `Ui` rather than loose fields:
// vim off is the absence of this, not a flag beside it.
pub(super) struct Vim {
    pub(super) mode: Mode,
    // Two chars that leave Insert, resolved once from config. `None` means
    // no valid sequence configured — no way into Normal at all.
    pub(super) escape: Option<(char, char)>,
    pub(super) window: std::time::Duration,
    // Last char typed, and when. Lazy like double-taps: nothing is held
    // pending, so the line is never a guess at a key that hasn't arrived.
    pub(super) last: Option<(char, Instant)>,
}

impl Vim {
    pub(super) fn new(cfg: &pi_store::config::Vim) -> Self {
        let mut vim = Self {
            mode: Mode::Insert,
            escape: None,
            window: std::time::Duration::ZERO,
            last: None,
        };
        vim.configure(cfg);
        vim
    }

    // Resolves the escape sequence from config once, not per keystroke.
    // Anything but exactly two chars leaves Normal deliberately unreachable.
    pub(super) fn configure(&mut self, cfg: &pi_store::config::Vim) {
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
