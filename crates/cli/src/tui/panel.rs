//! The panel: the settings rows drawn over the menu, one open at a time.
//!
//! It rides the existing menu plumbing — `MenuNext` / `MenuPrevious` move,
//! `MenuAccept` edits and submits, `MenuDismiss` cancels or closes — so a
//! user's own bindings follow without a second table; the browsing keys
//! (`j`, `i`, `q`, space, `r`) are a fixed vocabulary beside it. The cursor,
//! the edit line and the dispatch are written here once.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use std::time::{Duration, Instant};

use super::Paint;
use super::editor::Editor;
use super::screen;
use crate::icons;
use crate::keys::Action;
use crate::repl::{Intent, mask_secret};
use crate::settings::SettingRow;

/// The rows a panel shows: every path the config has, the value in force for
/// the session, and whether the file still holds another one.
pub struct Panel {
    rows: Vec<SettingRow>,
    at: usize,
    // The pair that leaves Insert, read once from the config; `None` — an
    // empty setting, or any other length — is no sequence, and Enter and Esc
    // are then the only way out.
    escape: Option<(char, char)>,
    window: Duration,
    // The character that may be the pair's first half, and when it landed.
    // Lazy like the editor's: the character is on screen already, so nothing
    // is held pending and the line is never a guess about a key that has not
    // arrived.
    pair: Option<(char, Instant)>,
    // Some while a row is being rewritten; browsing otherwise.
    editing: Option<Editor>,
    // What the last commit refused, shown under the rows.
    refused: Option<String>,
}

/// What the panel made of a press.
pub enum Took {
    // Handled, and this is what it asks the loop for.
    Intent(Intent),
    // Handled, and the panel is done: the caller drops it.
    Close,
}

impl Panel {
    pub fn new(rows: Vec<SettingRow>, vim: &crate::config::Vim) -> Self {
        Self {
            rows,
            at: 0,
            escape: vim.escape_pair(),
            window: Duration::from_millis(vim.escape_timeout_ms),
            pair: None,
            editing: None,
            refused: None,
        }
    }

    // Normal mode, no edit up: where `j`, `i`, `q` and the row's own verbs
    // are keys rather than letters. The panel has no Normal that outlives an
    // edit — Insert is exactly the time one is open.
    fn browsing(&self) -> bool {
        self.editing.is_none()
    }

    /// Only the tests ask whether a row is being rewritten.
    #[cfg(test)]
    pub fn editing(&self) -> bool {
        self.editing.is_some()
    }

    /// What is being typed, or the focused row's own text when nothing is.
    pub fn editing_value(&self) -> &str {
        match &self.editing {
            Some(e) => e.text(),
            None => self.rows.get(self.at).map_or("", |r| r.value.as_str()),
        }
    }

    /// Replace the rows after a commit changed what they are a view of, and
    /// close the edit that changed them. The panel lands back in Normal mode:
    /// the commit kept, the panel browsed.
    pub fn refresh(&mut self, rows: Vec<SettingRow>) {
        // The cursor stays on the path it was on, wherever that moved to: the
        // config's rows are a set rather than a list.
        self.at = self
            .rows
            .get(self.at)
            .and_then(|row| rows.iter().position(|n| n.path == row.path))
            .unwrap_or(0);
        self.rows = rows;
        self.editing = None;
        self.refused = None;
    }

    /// Show what the last commit refused, keeping the edit open.
    pub fn refuse(&mut self, why: String) {
        self.refused = Some(why);
    }

    // Enter the editing state for the focused row, pre-filled with what it
    // holds. The secret value shows in clear here: a pasted key has to be
    // checkable. A panel with nothing in it has nothing to edit.
    fn begin_edit(&mut self) {
        let Some(row) = self.rows.get(self.at) else {
            return;
        };
        let mut e = Editor::default();
        e.set_line(&row.value);
        self.editing = Some(e);
        self.refused = None;
    }

    fn down(&mut self) {
        self.at = (self.at + 1).min(self.rows.len().saturating_sub(1));
    }

    fn up(&mut self) {
        self.at = self.at.saturating_sub(1);
    }

    // A character while Insert is up: the pair's second half closes the edit
    // (the first half comes back off the line), anything else arms or clears
    // the pair and lands in the line.
    fn typed_char(&mut self, c: char) -> bool {
        let escape = self.escape;
        let closed = match escape {
            Some((first, second)) if c == second => self
                .pair
                .take()
                .is_some_and(|(p, at)| p == first && at.elapsed() < self.window),
            _ => false,
        };
        if !closed {
            self.pair = escape
                .filter(|(first, _)| c == *first)
                .map(|_| (c, Instant::now()));
        }
        closed
    }

    // Leave Insert with the edit kept. A value that did not change is just
    // dropped; one that did goes to the session as a claim, and the panel
    // stays in Insert until the commit answers — `refresh` closes it,
    // `refuse` keeps it open beside the refusal.
    fn submit_edit(&mut self) -> Took {
        let text = self.editing_value().to_string();
        let changed = self.rows.get(self.at).is_some_and(|r| r.value != text);
        if !changed {
            self.editing = None;
            self.refused = None;
            return Took::Intent(Intent::None);
        }
        let path = self.rows[self.at].path.clone();
        Took::Intent(Intent::SettingEdit(path, text))
    }

    /// What this press does to the panel, or `Nothing` where the panel has no
    /// answer for it and the editor underneath should.
    pub fn press(&mut self, bound: Option<Action>, key: KeyEvent) -> Took {
        // A press that means something breaks a half-typed escape pair, the
        // way it breaks the editor's.
        if bound.is_some() {
            self.pair = None;
        }
        // Normal mode's keys come before the bound table: `j`, `i`, `q` and
        // the row's own verbs are a fixed vocabulary, not a rebindable layer —
        // that is what makes them mode keys rather than letters. Bare letters
        // only: ctrl and alt keep a letter a letter, as they always did under
        // the bound table this vocabulary replaced.
        if self.browsing()
            && let KeyCode::Char(c) = key.code
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            match c {
                'j' => {
                    self.down();
                    return Took::Intent(Intent::None);
                }
                'k' => {
                    self.up();
                    return Took::Intent(Intent::None);
                }
                'i' | 'e' => {
                    self.begin_edit();
                    return Took::Intent(Intent::None);
                }
                'q' => return Took::Close,
                // Space writes the session value to the file, r takes it
                // back; the settings panel owns both, on a changed row.
                ' ' => {
                    return Took::Intent(
                        self.rows
                            .get(self.at)
                            .map(|r| Intent::SettingWrite(r.path.clone()))
                            .unwrap_or(Intent::None),
                    );
                }
                'r' => {
                    return Took::Intent(
                        self.rows
                            .get(self.at)
                            .map(|r| Intent::SettingRevert(r.path.clone()))
                            .unwrap_or(Intent::None),
                    );
                }
                _ => {}
            }
        }
        match bound {
            // The cursor holds still while a row is being typed: `j` is a
            // letter then, and the layer that binds it is off.
            Some(Action::MenuNext) => {
                if self.editing.is_none() {
                    self.down();
                }
                Took::Intent(Intent::None)
            }
            Some(Action::MenuPrevious) => {
                if self.editing.is_none() {
                    self.up();
                }
                Took::Intent(Intent::None)
            }
            Some(Action::MenuAccept) => {
                if self.editing.is_none() {
                    self.begin_edit();
                    return Took::Intent(Intent::None);
                }
                self.submit_edit()
            }
            // An edit in progress goes first: the panel closes only once there
            // is nothing left inside it to cancel. Discarding lands back in
            // Normal mode with the row as it was.
            Some(Action::MenuDismiss | Action::LineClear) => match self.editing.take() {
                Some(_) => {
                    self.refused = None;
                    Took::Intent(Intent::None)
                }
                None => Took::Close,
            },
            _ => {
                // A character either completes the escape pair — which takes
                // its first half back off and keeps the edit — or lands in
                // the line. The pair is decided before the editor is
                // borrowed, so the two halves of the state never argue; and
                // only an edit can arm one, so a browsing letter never leaves
                // a half behind for a later edit to trip on.
                let typed = if let KeyCode::Char(c) = key.code
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    Some(c)
                } else {
                    None
                };
                let closed = self.editing.is_some() && typed.is_some_and(|c| self.typed_char(c));
                if let Some(e) = &mut self.editing {
                    match bound {
                        Some(Action::DeleteCharBack) => e.backspace(),
                        Some(Action::DeleteCharForward) => e.delete(),
                        Some(Action::DeleteWordBack) => e.kill_word_back(),
                        Some(Action::DeleteToLineEnd) => e.kill_to_end(),
                        Some(Action::DeleteToLineStart) => e.kill_to_start(),
                        Some(Action::MoveCharLeft) => e.left(),
                        Some(Action::MoveCharRight) => e.right(),
                        Some(Action::MoveWordLeft) => e.word_left(),
                        Some(Action::MoveWordRight) => e.word_right(),
                        Some(Action::MoveLineStart) => e.home(),
                        Some(Action::MoveLineEnd) => e.end(),
                        _ if closed => {
                            e.backspace();
                        }
                        _ if let Some(c) = typed => {
                            e.insert(c);
                        }
                        _ => {}
                    }
                }
                if closed {
                    return self.submit_edit();
                }
                Took::Intent(Intent::None)
            }
        }
    }

    /// The rows to paint, the selected one marked, then whatever the last
    /// commit refused.
    ///
    /// Wrapped here rather than left to `Rows`: the panel is sized by counting
    /// rows, and a row `Rows` wrapped on its own is a row the count misses.
    /// The row being rewritten is painted as itself — its value sits on the
    /// row, in the clear — and the caret rides it at the value's own cursor
    /// offset, wrapped like the input line.
    pub fn view(&self, paint: &Paint, width: usize) -> (Vec<String>, Option<(u16, u16)>) {
        let mut out = Vec::new();
        let mut caret = None;
        for i in 0..self.rows.len() {
            let editing_this = self.editing.is_some() && i == self.at;
            let caret_row = out.len() as u16;
            if editing_this {
                // The row being rewritten, in the clear, styled like the
                // input line. The rest of the rows stand as they were, their
                // selection dropped for the moment the caret owns the row.
                let editor = self.editing.as_ref().expect("editing this row");
                let row = &self.rows[i];
                let line = format!("{} {} = {}", icons::MENU_SIGIL, row.path, editor.text());
                // The lead — sigil, path, ` = ` — is whatever is really on
                // the line: its width is measured from it, never assumed, and
                // the caret's byte offset rides after the value's own.
                let lead_bytes = icons::MENU_SIGIL.len() + 1 + row.path.len() + 3;
                let lead_w = crate::render::visible_width(&line[..lead_bytes]);
                let caret_at = lead_bytes + editor.cursor();
                let (lines, in_row, col) = wrap_edit(&line, lead_w, caret_at, width);
                let painted: Vec<String> = lines
                    .into_iter()
                    .map(|l| paint.on(&paint.theme.input, &l))
                    .collect();
                out.extend(painted);
                caret = Some((caret_row + in_row as u16, col as u16));
            } else {
                let caret_sigil = if i == self.at { icons::MENU_SIGIL } else { " " };
                let line = format!("{caret_sigil} {}", self.row(i));
                let styled = if i == self.at {
                    paint.on(&paint.theme.menu.selected, &line)
                } else {
                    line
                };
                out.extend(screen::fit(&styled, width));
            }
        }
        if let Some(why) = &self.refused {
            out.extend(screen::fit(&format!("  {} {why}", icons::FAIL_MARK), width));
        }
        // The one line of chrome: what mode is up and what its keys are. A
        // panel that answers to q and space says so, or the first q lands as
        // a mystery.
        let line = if self.browsing() {
            format!(
                "  normal{}j/k move · i edit · space write file · r revert · q close",
                icons::KEY_NOTE_SEP
            )
        } else {
            let keep = self
                .escape
                .map_or_else(|| "enter".to_string(), |(a, b)| format!("enter or {a}{b}"));
            format!("  insert{}{keep} keeps · esc discards", icons::KEY_NOTE_SEP)
        };
        out.extend(screen::fit(&line, width));
        (out, caret)
    }

    // One row as it paints, without the caret the list puts in front of it.
    // Secrets show as set-or-not; only the row being rewritten carries the
    // clear text, where a pasted key has to be checkable.
    fn row(&self, at: usize) -> String {
        let row = &self.rows[at];
        let shown = mask_secret(&row.path, &row.value);
        // The session has left the file's value behind: the mark says so, and
        // normal mode's space and r are what settle it.
        let mark = if row.changed {
            format!(" {}", icons::CHANGED_MARK)
        } else {
            String::new()
        };
        format!("{} = {shown}{mark}", row.path)
    }
}

// The row being rewritten, wrapped like the input line: continuation rows
// indent under the lead (measured from the line itself), and the caret lands
// on the row its byte offset wraps onto, its column the display columns
// before it there. `lead_w` is the lead's width in columns; `caret` is the
// byte offset into `line`. `used` counts absolute columns: the lead already
// on the row, the value after it.
fn wrap_edit(line: &str, lead_w: usize, caret: usize, width: usize) -> (Vec<String>, usize, usize) {
    use unicode_width::UnicodeWidthChar;
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0usize;
    let mut caret_at = (0usize, 0usize);
    for (i, c) in line.char_indices() {
        let w = c.width().unwrap_or(0);
        if used + w > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            used = lead_w;
            row.push_str(&" ".repeat(lead_w));
        }
        // After the wrap, so a caret exactly on the break lands at the
        // start of the new row rather than off the end of the old one.
        if i == caret {
            caret_at = (rows.len(), used);
        }
        row.push(c);
        used += w;
    }
    let (caret_row, caret_col) = if caret == line.len() {
        (rows.len(), used)
    } else {
        caret_at
    };
    rows.push(row);
    (rows, caret_row, caret_col)
}

#[cfg(test)]
mod tests {
    use super::{Paint, Panel, Took};
    use crate::keys::Action;
    use crate::repl::Intent;
    use crate::settings::{SettingRow, row};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn act(panel: &mut Panel, action: Action) -> Took {
        panel.press(
            Some(action),
            KeyEvent::new(KeyCode::Null, KeyModifiers::NONE),
        )
    }

    fn typed(panel: &mut Panel, text: &str) {
        for c in text.chars() {
            panel.press(None, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    fn intent(took: Took) -> Intent {
        match took {
            Took::Intent(i) => i,
            Took::Close => panic!("the panel closed"),
        }
    }

    fn close(took: Took) {
        assert!(matches!(took, Took::Close), "the panel did not close");
    }

    fn typed_close(panel: &mut Panel, c: char) -> Took {
        panel.press(None, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    // The default vim settings: escape `jk`, the window the config ships with.
    fn vim() -> crate::config::Vim {
        <crate::config::Vim as Default>::default()
    }

    fn settings() -> Vec<SettingRow> {
        vec![
            row("base_url", "http://x", false),
            row("model", "flash", true),
        ]
    }

    #[test]
    fn an_empty_panel_has_nothing_to_edit() {
        let mut p = Panel::new(Vec::new(), &vim());
        act(&mut p, Action::MenuAccept);
        assert!(!p.editing());
        assert_eq!(p.editing_value(), "", "rather than indexing past the end");
    }

    #[test]
    fn modifiers_keep_the_browsing_letters_dead() {
        // Ctrl+R is not `r`: the fixed vocabulary answers bare letters only,
        // the way the bound table it replaced always did.
        let mut p = Panel::new(settings(), &vim());
        let took = p.press(
            None,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        );
        assert!(matches!(took, Took::Intent(Intent::None)), "not a revert");
        assert!(!matches!(
            p.press(
                None,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)
            ),
            Took::Close
        ));
    }

    #[test]
    fn a_pair_arms_under_an_edit_and_not_beside_one() {
        // `hl` as the pair: an `h` while browsing arms nothing, so `i` opens
        // the edit with the value whole and `l` lands as a letter — not as
        // the close that eats the value's last character and submits.
        let vim = crate::config::Vim {
            escape: "hl".into(),
            ..vim()
        };
        let mut p = Panel::new(settings(), &vim);
        typed(&mut p, "h");
        p.press(None, KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        let whole = p.editing_value().to_string();
        typed(&mut p, "l");
        assert!(p.editing(), "the edit is still open, not submitted");
        assert_eq!(p.editing_value(), format!("{whole}l"));
    }

    // The panel opens browsing: j and k move, i opens the row, q closes.
    #[test]
    fn normal_mode_moves_and_closes() {
        let mut p = Panel::new(settings(), &vim());
        typed(&mut p, "j");
        assert_eq!(p.editing_value(), "flash", "j moved down");
        typed(&mut p, "k");
        assert_eq!(p.editing_value(), "http://x", "k moved back up");
        close(typed_close(&mut p, 'q'));
    }

    // i opens the row; the escape pair takes its first half back off and
    // keeps the edit as the session's. The panel stays in Insert until the
    // commit answers — the loop's refresh closes it.
    #[test]
    fn i_opens_the_row_and_jk_keeps_it() {
        let mut p = Panel::new(settings(), &vim());
        typed(&mut p, "ji");
        assert!(p.editing());
        assert_eq!(p.editing_value(), "flash", "pre-filled with what it holds");
        typed(&mut p, "yj");
        match intent(typed_close(&mut p, 'k')) {
            Intent::SettingEdit(path, value) => {
                assert_eq!((path.as_str(), value.as_str()), ("model", "flashy"));
            }
            other => panic!("kept {other:?}"),
        }
        assert!(p.editing(), "the commit answers before Insert closes");
    }

    // Space and r settle a row the session and the file disagree on.
    #[test]
    fn space_and_r_settle_a_row() {
        let mut p = Panel::new(settings(), &vim());
        typed(&mut p, "j");
        assert!(matches!(
            intent(typed_close(&mut p, ' ')),
            Intent::SettingWrite(ref path) if path == "model"
        ));
        assert!(matches!(
            intent(typed_close(&mut p, 'r')),
            Intent::SettingRevert(ref path) if path == "model"
        ));
    }

    // Esc drops the edit and lands back in Normal mode with the row as it
    // was; the panel closes only from there.
    #[test]
    fn esc_discards_the_edit_before_it_leaves_the_panel() {
        let mut p = Panel::new(settings(), &vim());
        typed(&mut p, "ji");
        typed(&mut p, "y");
        assert!(matches!(
            act(&mut p, Action::MenuDismiss),
            Took::Intent(Intent::None)
        ));
        assert!(!p.editing());
        assert_eq!(p.editing_value(), "flash", "the row as it was");
        assert!(matches!(act(&mut p, Action::MenuDismiss), Took::Close));
    }

    // An edit that changed nothing asks for nothing: leaving Insert just
    // closes the edit line.
    #[test]
    fn an_unchanged_edit_keeps_nothing() {
        let mut p = Panel::new(settings(), &vim());
        act(&mut p, Action::MenuAccept);
        assert_eq!(p.editing_value(), "http://x", "pre-filled, untouched");
        assert!(matches!(
            intent(act(&mut p, Action::MenuAccept)),
            Intent::None
        ));
        assert!(!p.editing());
    }

    #[test]
    fn accepting_a_row_opens_it_and_accepting_again_commits_it() {
        let mut p = Panel::new(settings(), &vim());
        act(&mut p, Action::MenuNext);
        act(&mut p, Action::MenuAccept);
        assert_eq!(p.editing_value(), "flash", "pre-filled with what it holds");
        typed(&mut p, "y");
        match intent(act(&mut p, Action::MenuAccept)) {
            Intent::SettingEdit(path, value) => {
                assert_eq!((path.as_str(), value.as_str()), ("model", "flashy"));
            }
            other => panic!("committed {other:?}"),
        }
    }

    #[test]
    fn refresh_keeps_the_cursor_on_the_same_path() {
        let mut p = Panel::new(settings(), &vim());
        act(&mut p, Action::MenuNext);
        let after = vec![
            row("model", "deepseek", false),
            row("base_url", "http://y", false),
        ];
        p.refresh(after);
        assert_eq!(
            p.editing_value(),
            "deepseek",
            "the same path, wherever it moved to"
        );
        act(&mut p, Action::MenuAccept);
        typed(&mut p, "x");
        assert!(matches!(
            intent(act(&mut p, Action::MenuAccept)),
            Intent::SettingEdit(ref path, _) if path == "model"
        ));
    }

    // The cursor moves only while a row is being browsed: `j` is a letter
    // the moment a value is being typed.
    #[test]
    fn typing_a_value_pins_the_cursor() {
        let mut p = Panel::new(settings(), &vim());
        act(&mut p, Action::MenuAccept);
        assert_eq!(p.editing_value(), "http://x");
        act(&mut p, Action::MenuNext);
        act(&mut p, Action::MenuNext);
        typed(&mut p, "!");
        assert!(matches!(
            intent(act(&mut p, Action::MenuAccept)),
            Intent::SettingEdit(ref path, ref t) if path == "base_url" && t == "http://x!"
        ));
    }

    // Dismiss is two steps: the edit first, the panel only once there is
    // nothing left inside it to cancel.
    #[test]
    fn dismiss_leaves_the_edit_before_it_leaves_the_panel() {
        let mut p = Panel::new(settings(), &vim());
        act(&mut p, Action::MenuAccept);
        assert!(matches!(
            act(&mut p, Action::MenuDismiss),
            Took::Intent(Intent::None)
        ));
        assert!(!p.editing());
        assert!(matches!(act(&mut p, Action::MenuDismiss), Took::Close));
    }

    #[test]
    fn a_browsing_panel_swallows_a_key_it_does_not_know() {
        let mut p = Panel::new(settings(), &vim());
        let press = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(matches!(p.press(None, press), Took::Intent(Intent::None)));
    }

    #[test]
    fn only_the_focused_row_is_highlighted() {
        let mut p = Panel::new(
            vec![
                row("a", "1", false),
                row("b", "2", false),
                row("c", "3", false),
            ],
            &vim(),
        );
        let paint = Paint::new(true);
        let (rows, caret) = p.view(&paint, 80);
        assert!(caret.is_none());
        assert!(rows[0].contains("\x1b[7m"));
        assert!(!rows[1].contains("\x1b[7m"));
        assert!(!rows[2].contains("\x1b[7m"));

        act(&mut p, Action::MenuNext);
        let (rows, caret) = p.view(&paint, 80);
        assert!(caret.is_none());
        assert!(!rows[0].contains("\x1b[7m"));
        assert!(rows[1].contains("\x1b[7m"));
        assert!(!rows[2].contains("\x1b[7m"));

        act(&mut p, Action::MenuAccept);
        let (rows, caret) = p.view(&paint, 80);
        assert!(caret.is_some());
        assert!(!rows[1].contains("\x1b[7m"));
    }

    // The row being rewritten is the edit line: painted like the input, the
    // caret riding it after the value, and no separate line below.
    #[test]
    fn the_row_being_rewritten_is_the_edit_line() {
        let mut p = Panel::new(settings(), &vim());
        typed(&mut p, "i");
        typed(&mut p, "z");
        let (rows, caret) = p.view(&Paint::new(true), 80);
        let (row, col) = caret.expect("the caret rides the edited row");
        assert_eq!(row, 0, "on the row itself, not a line below");
        assert_eq!(
            col, 22,
            "one column past the value, at the end of `base_url = http://xz`"
        );
        assert!(
            rows[0].contains("base_url = http://xz"),
            "the row is the line: {}",
            rows[0]
        );
        assert!(
            !rows[0].contains("\x1b[7m"),
            "the row's selection stands down for the caret"
        );
        assert_eq!(rows.len(), 3, "rows, then the footer — no edit line below");
    }

    // The caret's column is measured from what is really on the line, so a
    // wide path or a wide value cannot push it off the value's end — the
    // bug class this whole rendering exists to rule out.
    #[test]
    fn the_caret_survives_wide_paths_and_values() {
        let mut p = Panel::new(vec![row("模型.name", "http://x", false)], &vim());
        typed(&mut p, "i");
        typed(&mut p, "中");
        let (rows, caret) = p.view(&Paint::new(true), 80);
        let (r, col) = caret.expect("the caret rides the edited row");
        assert_eq!(r, 0);
        assert_eq!(
            col as usize,
            crate::render::visible_width("› 模型.name = http://x中"),
            "one column past the value, however wide the path"
        );
        assert!(rows[0].contains("模型.name = http://x中"));
    }

    // A value longer than the row wraps, the continuation indented under the
    // lead like the input line, and the caret lands on the row it wraps onto.
    #[test]
    fn a_wrapped_value_keeps_the_caret_on_its_own_row() {
        let mut p = Panel::new(settings(), &vim());
        typed(&mut p, "i");
        let long = "x".repeat(100);
        typed(&mut p, &long);
        let (rows, caret) = p.view(&Paint::new(true), 24);
        let (r, col) = caret.expect("the caret rides the edited row");
        // The caret row's own rendered width is exactly where the caret sits:
        // a wrapped value may land the caret anywhere, but never past the
        // text actually on its row.
        assert_eq!(
            crate::render::visible_width(&rows[r as usize]),
            col as usize,
            "the caret is at the end of its own row"
        );
        assert!(r > 0, "the value wrapped");
    }

    // A row the session has left behind carries the mark, so normal mode's
    // space and r have something to point at.
    #[test]
    fn a_row_the_session_left_the_file_carries_the_mark() {
        let p = Panel::new(settings(), &vim());
        let (rows, _) = p.view(&Paint::new(true), 80);
        assert!(
            !rows[0].contains(crate::icons::CHANGED_MARK),
            "the file agrees on {}",
            rows[0]
        );
        assert!(
            rows[1].contains(crate::icons::CHANGED_MARK),
            "the file does not hold {}",
            rows[1]
        );
    }
}
