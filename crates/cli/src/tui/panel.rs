//! The panel: rows drawn over the menu, one open at a time.
//!
//! `/settings` and `/mem` used to be an `Option` field each, and "one at a
//! time" an invariant six places kept by hand; a seventh that forgot one
//! would leave two panels fighting over the same keys. One field holding one
//! `Body` makes that the type's job.
//!
//! It rides the existing menu plumbing — `MenuNext` / `MenuPrevious` move,
//! `MenuAccept` edits and submits, `MenuDismiss` cancels or closes — so a
//! user's own bindings follow without a second table. What a panel has of its
//! own is the six matches below; the cursor, the edit line and the dispatch
//! are written here once.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::Paint;
use super::editor::Editor;
use super::screen;
use crate::icons;
use crate::journal;
use crate::keys::{Action, Which};
use crate::memory;
use crate::repl::Intent;

/// Which panel is open, and the rows it is showing. A new panel is a variant
/// here, and the matches below are what the compiler then asks it to answer.
pub enum Body {
    // Every path the config has, and the value it holds.
    Settings(Vec<(String, String)>),
    // What this workspace remembers.
    Shelf(Vec<memory::Row>),
}

impl Body {
    fn len(&self) -> usize {
        match self {
            Body::Settings(rows) => rows.len(),
            Body::Shelf(rows) => rows.len(),
        }
    }

    // One row as it paints, without the caret the list puts in front of it.
    // `hidden` marks the row being rewritten: the edit line below carries its
    // text, so the row itself stands aside.
    fn row(&self, at: usize, hidden: bool) -> String {
        match self {
            Body::Settings(rows) => {
                let (path, value) = &rows[at];
                // Set-or-not in the list; only the edit line carries the
                // clear text, where a pasted key has to be checkable.
                let shown = if hidden {
                    icons::ELLIPSIS.to_string()
                } else if journal::secret(journal::leaf(path)) {
                    if value.is_empty() {
                        "<unset>".to_string()
                    } else {
                        "<set>".to_string()
                    }
                } else {
                    value.clone()
                };
                format!("{path} = {shown}")
            }
            Body::Shelf(rows) => {
                let row = &rows[at];
                let shown = if hidden { icons::ELLIPSIS } else { &row.text };
                format!("{}  {shown}", row.day)
            }
        }
    }

    // The text an edit of this row starts from, or None where the cursor is
    // on nothing: a panel can be empty, and an empty one has no row.
    fn text(&self, at: usize) -> Option<&str> {
        match self {
            Body::Settings(rows) => rows.get(at).map(|(_, v)| v.as_str()),
            Body::Shelf(rows) => rows.get(at).map(|r| r.text.as_str()),
        }
    }

    // What a finished edit asks the loop for.
    fn commit(&self, at: usize, text: String) -> Option<Intent> {
        match self {
            Body::Settings(rows) => rows
                .get(at)
                .map(|(path, _)| Intent::CommitSetting(path.clone(), text)),
            Body::Shelf(rows) => rows.get(at).map(|r| Intent::ShelfWrite(r.id, text)),
        }
    }

    // What taking the focused row away asks for, where rows can go at all.
    // The config has no row to take away, and `x` is not bound over it.
    fn remove(&self, at: usize) -> Option<Intent> {
        match self {
            Body::Settings(_) => None,
            Body::Shelf(rows) => rows.get(at).map(|r| Intent::ShelfDrop(r.id)),
        }
    }

    // The layer this panel's own verbs sit under, over the menu bindings it
    // borrows. None where it has no verbs of its own.
    fn layer(&self) -> Option<Which> {
        match self {
            Body::Settings(_) => None,
            Body::Shelf(_) => Some(Which::Shelf),
        }
    }

    // What to say when there are no rows, where an empty panel would
    // otherwise be an empty space.
    fn empty(&self) -> Option<&'static str> {
        match self {
            Body::Settings(_) => None,
            Body::Shelf(_) => {
                Some("  nothing on the shelf here — `/mem <what to keep>` puts something on it")
            }
        }
    }

    /// Whether the panel belongs to the checkout in front. The shelf does:
    /// left open across a switch it would go on showing the notes of the tree
    /// behind you, and the next `x` would take one off the shelf of the tree
    /// in front. The config is the machine's, and stays.
    pub fn lane_scoped(&self) -> bool {
        match self {
            Body::Settings(_) => false,
            Body::Shelf(_) => true,
        }
    }

    // Where the cursor goes when the rows are re-read. It stays where the eye
    // is — on whatever took the place of the row that went — except over the
    // config, whose rows are a set rather than a list: there it follows the
    // path it was on.
    fn keep(&self, at: usize, new: &Body) -> usize {
        match (self, new) {
            (Body::Settings(old), Body::Settings(new)) => old
                .get(at)
                .and_then(|(path, _)| new.iter().position(|(p, _)| p == path))
                .unwrap_or(0),
            _ => at.min(new.len().saturating_sub(1)),
        }
    }
}

/// A panel over the menu: a cursor over rows, and an edit line over the row
/// under it.
pub struct Panel {
    body: Body,
    at: usize,
    // Some while a row is being rewritten; browsing otherwise.
    editing: Option<Editor>,
    // What the last commit refused, shown under the rows.
    refused: Option<String>,
}

/// What the panel made of a press.
pub enum Took {
    // Not the panel's key: the editor underneath it gets it.
    Nothing,
    // Handled, and this is what it asks the loop for.
    Intent(Intent),
    // Handled, and the panel is done: the caller drops it.
    Close,
}

impl Panel {
    pub fn new(body: Body) -> Self {
        Self {
            body,
            at: 0,
            editing: None,
            refused: None,
        }
    }

    pub fn body(&self) -> &Body {
        &self.body
    }

    /// Only the tests ask: the surface reads `layer`, which is the same
    /// question in the form it acts on.
    #[cfg(test)]
    pub fn editing(&self) -> bool {
        self.editing.is_some()
    }

    /// The key layer this panel raises. Off while a row is being rewritten, so
    /// that verbs like `e` and `x` are letters again.
    pub fn layer(&self) -> Option<Which> {
        match self.editing {
            Some(_) => None,
            None => self.body.layer(),
        }
    }

    /// What is being typed, or the focused row's own text when nothing is.
    pub fn editing_value(&self) -> &str {
        match &self.editing {
            Some(e) => e.text(),
            None => self.body.text(self.at).unwrap_or(""),
        }
    }

    /// Replace the rows after a commit changed what they are a view of, and
    /// close the edit that changed them.
    pub fn refresh(&mut self, body: Body) {
        self.at = self.body.keep(self.at, &body);
        self.body = body;
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
        let Some(text) = self.body.text(self.at) else {
            return;
        };
        let mut e = Editor::default();
        e.set_line(text);
        self.editing = Some(e);
        self.refused = None;
    }

    /// What this press does to the panel, or `Nothing` where the panel has no
    /// answer for it and the editor underneath should.
    pub fn press(&mut self, bound: Option<Action>, key: KeyEvent) -> Took {
        match bound {
            // The cursor holds still while a row is being typed: `j` is a
            // letter then, and the layer that binds it is off.
            Some(Action::MenuNext) => {
                if self.editing.is_none() {
                    self.at = (self.at + 1).min(self.body.len().saturating_sub(1));
                }
                Took::Intent(Intent::None)
            }
            Some(Action::MenuPrevious) => {
                if self.editing.is_none() {
                    self.at = self.at.saturating_sub(1);
                }
                Took::Intent(Intent::None)
            }
            Some(Action::MenuAccept) => {
                if self.editing.is_none() {
                    self.begin_edit();
                    return Took::Intent(Intent::None);
                }
                let text = self.editing_value().to_string();
                match self.body.commit(self.at, text) {
                    Some(intent) => Took::Intent(intent),
                    // Nothing under the cursor to write to; leave the edit.
                    None => {
                        self.editing = None;
                        self.refused = None;
                        Took::Intent(Intent::None)
                    }
                }
            }
            Some(Action::MenuDelete) => {
                Took::Intent(self.body.remove(self.at).unwrap_or(Intent::None))
            }
            // An edit in progress goes first: the panel closes only once there
            // is nothing left inside it to cancel.
            Some(Action::MenuDismiss) => match self.editing.take() {
                Some(_) => Took::Intent(Intent::None),
                None => Took::Close,
            },
            // Printable keys go into the edit line. While browsing the panel
            // has no answer, and the key falls through to the editor.
            _ if self.editing.is_some() => {
                if let Some(e) = &mut self.editing {
                    if let KeyCode::Char(c) = key.code
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        e.insert(c);
                    } else if matches!(bound, Some(Action::DeleteCharBack)) {
                        e.backspace();
                    }
                }
                Took::Intent(Intent::None)
            }
            _ => Took::Nothing,
        }
    }

    /// The rows to paint, the selected one marked, then the edit line and
    /// whatever the last commit refused.
    ///
    /// Wrapped here rather than left to `Rows`: the panel is sized by counting
    /// rows, and a row `Rows` wrapped on its own is a row the count misses.
    pub fn view(&self, paint: &Paint, width: usize) -> Vec<String> {
        if self.body.len() == 0
            && let Some(empty) = self.body.empty()
        {
            return screen::fit(empty, width);
        }
        let mut out = Vec::new();
        for i in 0..self.body.len() {
            let caret = if i == self.at { icons::MENU_SIGIL } else { " " };
            let hidden = self.editing.is_some() && i == self.at;
            let line = format!("{caret} {}", self.body.row(i, hidden));
            out.extend(screen::fit(
                &paint.on(&paint.theme.menu.selected, &line),
                width,
            ));
        }
        if let Some(editor) = &self.editing {
            let (line, _) = editor.view(paint, width);
            out.extend(line);
        }
        if let Some(why) = &self.refused {
            out.extend(screen::fit(&format!("  {} {why}", icons::FAIL_MARK), width));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{Body, Panel, Took};
    use crate::keys::Action;
    use crate::memory;
    use crate::repl::Intent;
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
            Took::Nothing => panic!("the panel let the press through"),
            Took::Close => panic!("the panel closed"),
        }
    }

    fn settings() -> Body {
        Body::Settings(vec![
            ("base_url".into(), "http://x".into()),
            ("model".into(), "flash".into()),
        ])
    }

    fn shelf(n: usize) -> Body {
        Body::Shelf(
            (0..n)
                .map(|i| memory::note(i as u64 + 1, format!("note {i}")))
                .collect(),
        )
    }

    #[test]
    fn an_empty_panel_has_nothing_to_edit() {
        let mut p = Panel::new(Body::Settings(Vec::new()));
        act(&mut p, Action::MenuAccept);
        assert!(!p.editing());
        assert_eq!(p.editing_value(), "", "rather than indexing past the end");
    }

    #[test]
    fn an_empty_shelf_has_nothing_to_rewrite_or_take_away() {
        let mut p = Panel::new(shelf(0));
        assert!(matches!(
            intent(act(&mut p, Action::MenuDelete)),
            Intent::None
        ));
        act(&mut p, Action::MenuNext);
        act(&mut p, Action::MenuAccept);
        assert!(!p.editing());
    }

    #[test]
    fn accepting_a_row_opens_it_and_accepting_again_commits_it() {
        let mut p = Panel::new(settings());
        act(&mut p, Action::MenuNext);
        act(&mut p, Action::MenuAccept);
        assert_eq!(p.editing_value(), "flash", "pre-filled with what it holds");
        typed(&mut p, "y");
        match intent(act(&mut p, Action::MenuAccept)) {
            Intent::CommitSetting(path, value) => {
                assert_eq!((path.as_str(), value.as_str()), ("model", "flashy"));
            }
            other => panic!("committed {other:?}"),
        }
    }

    #[test]
    fn refresh_keeps_the_config_cursor_on_the_same_path() {
        let mut p = Panel::new(settings());
        act(&mut p, Action::MenuNext);
        let after = Body::Settings(vec![
            ("model".into(), "deepseek".into()),
            ("base_url".into(), "http://y".into()),
        ]);
        p.refresh(after);
        assert_eq!(
            p.editing_value(),
            "deepseek",
            "the same path, wherever it moved to"
        );
        act(&mut p, Action::MenuAccept);
        assert!(matches!(
            intent(act(&mut p, Action::MenuAccept)),
            Intent::CommitSetting(ref path, _) if path == "model"
        ));
    }

    // Deleting the last row leaves the cursor on what is now last, rather
    // than one past the end where the panel would answer about nothing.
    #[test]
    fn the_shelf_cursor_survives_the_row_under_it_going() {
        let mut p = Panel::new(shelf(3));
        act(&mut p, Action::MenuNext);
        act(&mut p, Action::MenuNext);
        assert!(matches!(
            intent(act(&mut p, Action::MenuDelete)),
            Intent::ShelfDrop(3)
        ));
        p.refresh(shelf(2));
        assert!(
            matches!(
                intent(act(&mut p, Action::MenuDelete)),
                Intent::ShelfDrop(2)
            ),
            "the last row, not past it"
        );
    }

    // The cursor moves only while browsing, and the panel's own layer is off
    // then so that `j` reaches it as a letter.
    #[test]
    fn typing_a_note_pins_the_cursor_and_drops_the_layer() {
        let mut p = Panel::new(shelf(3));
        act(&mut p, Action::MenuAccept);
        assert_eq!(p.editing_value(), "note 0");
        assert_eq!(
            p.layer(),
            None,
            "its verbs are letters while a note is typed"
        );
        act(&mut p, Action::MenuNext);
        act(&mut p, Action::MenuNext);
        typed(&mut p, "!");
        assert!(matches!(
            intent(act(&mut p, Action::MenuAccept)),
            Intent::ShelfWrite(1, ref t) if t == "note 0!"
        ));
    }

    // Dismiss is two steps: the edit first, the panel only once there is
    // nothing left inside it to cancel.
    #[test]
    fn dismiss_leaves_the_edit_before_it_leaves_the_panel() {
        let mut p = Panel::new(shelf(1));
        act(&mut p, Action::MenuAccept);
        assert!(matches!(
            act(&mut p, Action::MenuDismiss),
            Took::Intent(Intent::None)
        ));
        assert!(!p.editing());
        assert!(matches!(act(&mut p, Action::MenuDismiss), Took::Close));
    }

    // A key the panel does not know goes on to the editor underneath, the
    // way it did when each panel had its own copy of this dispatch.
    #[test]
    fn a_browsing_panel_leaves_a_key_it_does_not_know_alone() {
        let mut p = Panel::new(shelf(1));
        let press = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(matches!(p.press(None, press), Took::Nothing));
    }
}
