//! The settings panel: every path the config has, and the one being typed.
//!
//! It rides the existing menu plumbing — `MenuNext` / `MenuPrevious` move,
//! `MenuAccept` edits and submits, `MenuDismiss` cancels or closes — so a
//! user's own key bindings follow without a second table.

use super::Paint;
use super::editor::Editor;
use crate::journal;

/// One row: the path and the value, the way a file would write it.
pub struct Panel {
    pub(crate) rows: Vec<(String, String)>,
    pub(crate) at: usize,
    /// Some while a value is being typed; browsing otherwise.
    editing: Option<Editor>,
    /// What the last commit refused, shown under the rows.
    error: Option<String>,
}

impl Panel {
    pub fn new(rows: Vec<(String, String)>) -> Self {
        Self {
            rows,
            at: 0,
            editing: None,
            error: None,
        }
    }

    pub fn editing(&self) -> bool {
        self.editing.is_some()
    }

    /// Enter the editing state for the selected row, pre-filled with the
    /// current value. The secret value shows in clear here: a pasted key has
    /// to be checkable. A panel with nothing in it has nothing to edit.
    pub fn begin_edit(&mut self) {
        let Some((_, value)) = self.rows.get(self.at) else {
            return;
        };
        let mut e = Editor::default();
        e.set_line(value);
        self.editing = Some(e);
        self.error = None;
    }

    /// The value being typed, or the current row's if none is.
    pub fn editing_value(&self) -> &str {
        match &self.editing {
            Some(e) => e.text(),
            None => &self.rows[self.at].1,
        }
    }

    /// A printable key goes into the edit line.
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

    /// The commit stuck; leave the edit and clear the error.
    pub fn finish_edit(&mut self) {
        self.editing = None;
        self.error = None;
    }
    /// Replace the rows after a commit changed the tree, keeping the
    /// selection on the row it was on when the path still exists.
    pub fn refresh(&mut self, rows: Vec<(String, String)>) {
        let selected = self.rows.get(self.at).map(|(p, _)| p.clone());
        self.rows = rows;
        self.at = selected
            .and_then(|p| self.rows.iter().position(|(r, _)| *r == p))
            .unwrap_or(0);
    }

    /// Show what the last commit refused, keeping the edit open.
    pub fn refuse(&mut self, why: String) {
        self.error = Some(why);
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

    /// The rows to paint: path + rendered value, the selected one marked, and
    /// the editing line if one is open. A secret value shows as `<set>` /
    /// `<unset>` in the list; only the editing line carries the clear text.
    pub fn view(&self, paint: &Paint, width: usize) -> Vec<String> {
        let mut out = Vec::new();
        for (i, (path, value)) in self.rows.iter().enumerate() {
            let caret = if i == self.at { "›" } else { " " };
            let shown = if self.editing.is_some() && i == self.at {
                "…".to_string()
            } else if journal::secret(journal::leaf(path)) {
                if value.is_empty() {
                    "<unset>".to_string()
                } else {
                    "<set>".to_string()
                }
            } else {
                value.clone()
            };
            let line = format!("{caret} {path} = {shown}");
            out.push(paint.on(&paint.theme.menu.selected, &line));
        }
        if let Some(editor) = &self.editing {
            let (line, _) = editor.view(paint, width);
            out.extend(line);
        }
        if let Some(err) = &self.error {
            out.push(format!("  ✗ {err}"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::Panel;

    fn rows() -> Vec<(String, String)> {
        vec![
            ("base_url".into(), "http://x".into()),
            ("model".into(), "flash".into()),
        ]
    }

    #[test]
    fn an_empty_panel_has_nothing_to_edit() {
        let mut p = Panel::new(Vec::new());
        p.begin_edit();
        assert!(!p.editing());
    }

    #[test]
    fn refresh_keeps_the_selection_on_the_same_path() {
        let mut p = Panel::new(rows());
        p.next();
        assert_eq!(p.at, 1);
        let after = rows()
            .into_iter()
            .map(|(path, _)| {
                let value = if path == "model" {
                    "deepseek".into()
                } else {
                    "http://y".into()
                };
                (path, value)
            })
            .collect();
        p.refresh(after);
        assert_eq!(p.at, 1, "the edited row stays selected");
        assert_eq!(p.rows[1].1, "deepseek", "the committed value shows");
    }
}
