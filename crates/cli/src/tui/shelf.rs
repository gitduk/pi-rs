//! The shelf panel: what this workspace remembers, and the note being rewritten.
//!
//! Movement and dismissal are the menu's own bindings, as the settings panel
//! does. Only the two verbs a shelf has and a menu does not — rewrite a row,
//! take one away — are its own, under `When::Shelf`, which is off while a note
//! is being typed so that `e` and `x` are letters again.

use super::Paint;
use super::editor::Editor;
use crate::memory::Row;

pub struct Panel {
    rows: Vec<Row>,
    at: usize,
    /// Some while a note is being rewritten; browsing otherwise.
    editing: Option<Editor>,
}

impl Panel {
    pub fn new(rows: Vec<Row>) -> Self {
        Self { rows, at: 0, editing: None }
    }

    pub fn editing(&self) -> bool {
        self.editing.is_some()
    }

    /// What to call the note under the cursor. A shelf can be empty, and an
    /// empty one has nothing to rewrite or take away.
    pub fn focused(&self) -> Option<u64> {
        self.rows.get(self.at).map(|r| r.id)
    }

    pub fn begin_edit(&mut self) {
        let Some(row) = self.rows.get(self.at) else {
            return;
        };
        let mut e = Editor::default();
        e.set_line(&row.text);
        self.editing = Some(e);
    }

    /// What is being typed, or the row's own text when nothing is.
    pub fn editing_value(&self) -> &str {
        match &self.editing {
            Some(e) => e.text(),
            None => self.rows.get(self.at).map(|r| r.text.as_str()).unwrap_or(""),
        }
    }

    pub fn insert(&mut self, c: char) {
        if let Some(e) = &mut self.editing {
            e.insert(c);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(e) = &mut self.editing {
            e.backspace();
        }
    }

    pub fn finish_edit(&mut self) {
        self.editing = None;
    }

    /// Replace the rows, keeping the cursor where the eye is: on whatever
    /// took the place of the row that went.
    pub fn refresh(&mut self, rows: Vec<Row>) {
        self.rows = rows;
        self.at = self.at.min(self.rows.len().saturating_sub(1));
    }

    /// Drop the edit in progress, or close the panel when browsing.
    pub fn dismiss(&mut self) -> bool {
        if self.editing.is_some() {
            self.editing = None;
            false
        } else {
            true
        }
    }

    pub fn next(&mut self) {
        if self.editing.is_none() {
            self.at = (self.at + 1).min(self.rows.len().saturating_sub(1));
        }
    }

    pub fn previous(&mut self) {
        if self.editing.is_none() {
            self.at = self.at.saturating_sub(1);
        }
    }

    pub fn view(&self, paint: &Paint, width: usize) -> Vec<String> {
        if self.rows.is_empty() {
            return vec!["  nothing on the shelf here — `/mem <what to keep>` puts something on it".into()];
        }
        let mut out = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            let caret = if i == self.at { "›" } else { " " };
            let shown = if self.editing.is_some() && i == self.at {
                "…"
            } else {
                &row.text
            };
            out.push(paint.on(
                &paint.theme.menu.selected,
                &format!("{caret} {}  {shown}", row.day),
            ));
        }
        if let Some(editor) = &self.editing {
            let (line, _) = editor.view(paint, width);
            out.extend(line);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{Panel, Row};

    fn rows(n: usize) -> Vec<Row> {
        (0..n)
            .map(|i| Row {
                id: i as u64 + 1,
                day: "2026-09-07".into(),
                text: format!("note {i}"),
            })
            .collect()
    }

    #[test]
    fn an_empty_shelf_has_nothing_to_rewrite_or_take_away() {
        let mut p = Panel::new(Vec::new());
        assert_eq!(p.focused(), None);
        p.begin_edit();
        assert!(!p.editing(), "nothing to edit");
        p.next();
        assert_eq!(p.focused(), None);
    }

    /// Deleting the last row leaves the cursor on what is now last, rather
    /// than one past the end where `at()` would answer None on a full shelf.
    #[test]
    fn the_cursor_survives_the_row_under_it_going() {
        let mut p = Panel::new(rows(3));
        p.next();
        p.next();
        assert_eq!(p.focused(), Some(3));
        p.refresh(rows(2));
        assert_eq!(p.focused(), Some(2), "the last row, not past it");

        p.refresh(Vec::new());
        assert_eq!(p.focused(), None);
    }

    /// The cursor moves only while browsing: `j` is a letter once a note is
    /// being typed, and the layer that binds it is off then.
    #[test]
    fn typing_a_note_pins_the_cursor() {
        let mut p = Panel::new(rows(3));
        p.begin_edit();
        assert_eq!(p.editing_value(), "note 0");
        p.next();
        p.next();
        assert_eq!(p.at, 0);
        p.finish_edit();
        p.next();
        assert_eq!(p.at, 1);
    }
}
