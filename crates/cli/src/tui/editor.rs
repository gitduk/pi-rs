//! The line being typed: a text buffer, a caret, and the history behind it.

use unicode_width::UnicodeWidthChar;

/// Columns the prompt marker occupies. Continuation rows are indented to match
/// so a wrapped line stays aligned under the first.
const GUTTER: usize = 2;
const CONT: &str = "  ";

#[derive(Default)]
pub struct Editor {
    text: String,
    /// Byte offset of the caret. Always on a char boundary.
    cursor: usize,
    history: Vec<String>,
    /// Where Up/Down currently sit. `history.len()` means the line being typed.
    at: usize,
    /// The line being typed, parked while history is being browsed.
    draft: String,
    /// The painted first-row gutter, so the theme can restyle it.
    prompt: String,
    /// The same gutter for a `!` line: the bang takes the prompt's place, so
    /// `!cmd` reads as a command rather than `› !cmd`.
    prompt_bang: String,
}

impl Editor {
    pub fn set_prompts(&mut self, prompt: String, bang: String) {
        self.prompt = prompt;
        self.prompt_bang = bang;
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replace the whole buffer, caret at the end. For accepting a completion.
    pub fn set_line(&mut self, line: &str) {
        self.text.clear();
        self.text.push_str(line);
        self.cursor = self.text.len();
    }

    /// Lines recalled with Up, oldest first.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn seed_history(&mut self, lines: Vec<String>) {
        self.history = lines;
        self.at = self.history.len();
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.at = self.history.len();
    }

    /// Hand over the line and remember it.
    pub fn take(&mut self) -> String {
        let line = std::mem::take(&mut self.text);
        self.cursor = 0;
        // A line identical to the last is not worth a second history slot.
        if !line.trim().is_empty() && self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        self.at = self.history.len();
        self.draft.clear();
        line
    }

    pub fn insert(&mut self, c: char) {
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn backspace(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
            self.text.remove(self.cursor);
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            self.text.remove(self.cursor);
        }
    }

    pub fn left(&mut self) {
        if let Some(c) = self.text[..self.cursor].chars().next_back() {
            self.cursor -= c.len_utf8();
        }
    }

    pub fn right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
        }
    }

    /// Skip the run of spaces first, then the word: the caret lands where the
    /// word starts rather than on the space in front of it.
    fn word_start(&self) -> usize {
        let head = &self.text[..self.cursor];
        let trimmed = head.trim_end_matches(char::is_whitespace);
        match trimmed.rfind(char::is_whitespace) {
            Some(i) => i + self.text[i..].chars().next().map_or(1, char::len_utf8),
            None => 0,
        }
    }

    fn word_end(&self) -> usize {
        let tail = &self.text[self.cursor..];
        let skipped = tail.len() - tail.trim_start_matches(char::is_whitespace).len();
        match tail[skipped..].find(char::is_whitespace) {
            Some(i) => self.cursor + skipped + i,
            None => self.text.len(),
        }
    }

    pub fn word_left(&mut self) {
        self.cursor = self.word_start();
    }

    pub fn word_right(&mut self) {
        self.cursor = self.word_end();
    }

    pub fn kill_word_back(&mut self) {
        let start = self.word_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// To the start of the current visual line, not of the whole buffer: a
    /// multi-line prompt otherwise has no way to reach a line's own start.
    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
    }

    pub fn end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i);
    }

    pub fn kill_to_end(&mut self) {
        let end = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i);
        self.text.replace_range(self.cursor..end, "");
    }

    pub fn kill_to_start(&mut self) {
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Older, unless the caret has somewhere to go within a multi-line buffer.
    pub fn up(&mut self) {
        if self.text.contains('\n') {
            return self.caret_up();
        }
        if self.at == 0 {
            return;
        }
        if self.at == self.history.len() {
            self.draft = self.text.clone();
        }
        self.at -= 1;
        self.text = self.history[self.at].clone();
        self.cursor = self.text.len();
    }

    pub fn down(&mut self) {
        if self.text.contains('\n') {
            return self.caret_down();
        }
        if self.at >= self.history.len() {
            return;
        }
        self.at += 1;
        self.text = match self.history.get(self.at) {
            Some(line) => line.clone(),
            None => std::mem::take(&mut self.draft),
        };
        self.cursor = self.text.len();
    }

    fn caret_up(&mut self) {
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        if start == 0 {
            return;
        }
        let col = self.cursor - start;
        let prev = self.text[..start - 1].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = floor_boundary(&self.text, (prev + col).min(start - 1));
    }

    fn caret_down(&mut self) {
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let col = self.cursor - start;
        let Some(nl) = self.text[self.cursor..].find('\n').map(|i| self.cursor + i) else {
            return;
        };
        let next_end = self.text[nl + 1..]
            .find('\n')
            .map_or(self.text.len(), |i| nl + 1 + i);
        self.cursor = floor_boundary(&self.text, (nl + 1 + col).min(next_end));
    }

    /// The rows to paint and where the caret sits among them.
    ///
    /// Wrapping is done here rather than left to the terminal: the live region
    /// is repainted by counting rows back, and a row the terminal wrapped on
    /// its own is a row the count does not know about.
    pub fn view(&self, width: usize) -> (Vec<String>, (u16, u16)) {
        // A line starting with `!` is a shell command; the bang takes the
        // prompt's place so the line reads `!cmd` rather than `› !cmd`. The
        // bare bang is one column, where the plain prompt's icon takes two
        // (icon plus space), so the gutter and continuation indent follow.
        let bang = self.text.starts_with('!');
        let body = if bang { &self.text[1..] } else { self.text.as_str() };
        let cursor = if bang {
            self.cursor.saturating_sub(1)
        } else {
            self.cursor
        };
        let gutter = if bang { GUTTER - 1 } else { GUTTER };

        let avail = width.saturating_sub(gutter).max(1);
        let mut rows: Vec<String> = Vec::new();
        let mut row = String::new();
        let mut used = 0usize;
        let mut caret = None;

        for (i, ch) in body.char_indices() {
            if ch == '\n' {
                if i == cursor {
                    caret = Some((rows.len() as u16, (gutter + used) as u16));
                }
                rows.push(std::mem::take(&mut row));
                used = 0;
                continue;
            }
            let w = ch.width().unwrap_or(0);
            if used + w > avail && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            // After the wrap, so a caret sitting exactly on the break lands at
            // the start of the new row rather than off the end of the old one.
            if i == cursor {
                caret = Some((rows.len() as u16, (gutter + used) as u16));
            }
            row.push(ch);
            used += w;
        }
        let caret = caret.unwrap_or((rows.len() as u16, (gutter + used) as u16));
        rows.push(row);

        let prompt = if bang {
            self.prompt_bang.as_str()
        } else {
            self.prompt.as_str()
        };
        // The continuation indent is one column under a bare-bang gutter so a
        // wrapped line stays aligned under the first, as the plain prompt's
        // two-column gutter keeps it in line with its own continuation.
        let cont = if bang { " " } else { CONT };
        let painted = rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| if i == 0 { prompt } else { cont }.to_string() + &r)
            .collect();
        (painted, caret)
    }
}

/// One entry per line, with the newlines inside an entry escaped, so a
/// multi-line prompt recalls as the one thing it was.
pub fn encode(entries: &[String]) -> String {
    entries
        .iter()
        .map(|e| e.replace('\\', "\\\\").replace('\n', "\\n") + "\n")
        .collect()
}

pub fn decode(body: &str) -> Vec<String> {
    body.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let mut out = String::with_capacity(l.len());
            let mut chars = l.chars();
            while let Some(c) = chars.next() {
                match (c, chars.clone().next()) {
                    ('\\', Some('n')) => {
                        chars.next();
                        out.push('\n');
                    }
                    ('\\', Some('\\')) => {
                        chars.next();
                        out.push('\\');
                    }
                    _ => out.push(c),
                }
            }
            out
        })
        .collect()
}

/// The nearest char boundary at or below `i`, so a column landing inside a
/// multi-byte character does not split it.
fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::Editor;
    /// An editor with the two painted gutters a real Ui would set, so view
    /// tests see the prompt a user would.
    fn typed(s: &str) -> Editor {
        let mut e = Editor::default();
        e.set_prompts("› ".into(), "!".into());
        e.insert_str(s);
        e
    }

    #[test]
    fn typing_and_backspacing_stay_on_char_boundaries() {
        let mut e = typed("中文abc");
        e.backspace();
        assert_eq!(e.text, "中文ab");
        e.left();
        e.backspace();
        // A byte-counting caret would split 文 and panic here.
        assert_eq!(e.text, "中文b");
    }

    #[test]
    fn a_word_delete_takes_the_space_in_front_of_the_word() {
        let mut e = typed("fix the bug");
        e.kill_word_back();
        assert_eq!(e.text, "fix the ");
        e.kill_word_back();
        assert_eq!(e.text, "fix ");
    }

    #[test]
    fn home_and_end_stay_within_one_line_of_a_multi_line_prompt() {
        let mut e = typed("first\nsecond");
        e.home();
        assert_eq!(e.text[e.cursor..].chars().next(), Some('s'));
        e.end();
        assert_eq!(e.cursor, e.text.len());
    }

    #[test]
    fn up_browses_history_but_moves_the_caret_when_there_are_lines_to_move_through() {
        let mut e = typed("older");
        e.take();
        let mut e2 = e;
        e2.insert_str("draft");
        e2.up();
        assert_eq!(e2.text, "older");
        e2.down();
        // The half-typed line comes back rather than being lost to the browse.
        assert_eq!(e2.text, "draft");

        let mut m = typed("one\ntwo");
        m.up();
        // Still the same buffer: Up moved within it instead of recalling.
        assert_eq!(m.text, "one\ntwo");
        assert!(m.cursor < 4);
    }

    #[test]
    fn a_bang_line_puts_the_bang_in_the_prompt() {
        let e = typed("!git status");
        let (rows, caret) = e.view(40);
        assert_eq!(rows[0], "!git status", "the bang takes the icon's place");
        assert_eq!(caret, (0, 11), "the bang column plus the ten characters");

        let plain = typed("git status");
        assert_eq!(plain.view(40).0[0], "› git status");
    }

    #[test]
    fn deleting_the_bang_returns_the_plain_prompt() {
        let mut e = typed("!git");
        e.home();
        e.delete();
        assert_eq!(e.view(40).0[0], "› git");
    }
    #[test]
    fn a_bang_line_wraps_with_the_bang_in_the_gutter() {
        let e = typed("!abcdefgh");
        // Width 6 leaves 5 columns after the one-column bang gutter, so the
        // body wraps exactly as it would under the plain two-column gutter.
        let (rows, caret) = e.view(6);
        assert_eq!(rows.len(), 2);
        assert_eq!(caret, (1, 4), "one gutter column + the last row's 3 chars");
    }

    #[test]
    fn history_does_not_keep_a_second_copy_of_a_repeated_line() {
        let mut e = typed("cargo test");
        e.take();
        e.insert_str("cargo test");
        e.take();
        assert_eq!(e.history.len(), 1);
    }

    #[test]
    fn a_wrapped_line_puts_the_caret_on_the_row_it_belongs_to() {
        let mut e = typed("abcdefgh");
        // Width 6 leaves 4 columns after the gutter.
        let (rows, caret) = e.view(6);
        assert_eq!(rows.len(), 2);
        assert_eq!(caret, (1, 6), "the caret is past the end of the second row");
        e.home();
        assert_eq!(e.view(6).1, (0, 2), "and back in the gutter's shadow");
    }

    #[test]
    fn a_caret_exactly_on_a_wrap_starts_the_next_row() {
        let mut e = typed("abcdefgh");
        for _ in 0..4 {
            e.left();
        }
        // Column 4 of a 4-wide row does not exist; it is column 0 of the next.
        assert_eq!(e.view(6).1, (1, 2));
    }

    #[test]
    fn an_explicit_newline_starts_a_row_of_its_own() {
        let (rows, caret) = typed("one\ntwo").view(40);
        assert_eq!(rows.len(), 2);
        assert!(rows[1].starts_with("  two"));
        assert_eq!(caret, (1, 5));
    }

    #[test]
    fn moving_between_lines_never_lands_inside_a_character() {
        let mut e = typed("中文中文\nx");
        e.end();
        e.up();
        // Landing mid-character would panic on the next insert.
        assert!(e.text.is_char_boundary(e.cursor));
        e.insert('!');
        assert!(e.text.contains('!'));
    }

    #[test]
    fn the_two_kills_cut_only_the_line_the_caret_is_on() {
        let mut e = typed("first\nsecond");
        e.home();
        e.right();
        e.kill_to_end();
        assert_eq!(e.text, "first\ns");
        e.kill_to_start();
        assert_eq!(e.text, "first\n");
    }

    #[test]
    fn history_survives_the_round_trip_through_a_file() {
        // A multi-line prompt is one entry, not several: read back a line at a
        // time it would come apart into fragments that recall separately.
        let entries = vec!["one".to_string(), "two\nlines".to_string()];
        assert_eq!(super::decode(&super::encode(&entries)), entries);
    }
}
