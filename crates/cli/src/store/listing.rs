//! The shape of a read-only answer: the rows a command has to show, before
//! anyone lays them out.
//!
//! Here rather than beside `Step`, which carries one, because the store is
//! where a shape several layers have to agree on lives — and a command table
//! fills one in too (`store/keys/mod.rs`), which `input` could not be reached
//! from without the two reading each other.

/// A read-only answer, as rows of cells.
///
/// Cells rather than lines because the layout belongs to whoever draws it: how
/// wide the columns end up is not knowable where the answer is built, and the
/// answer outlives the frame it was made in.
///
/// One shape for every such answer — `/keys`, `/help`, `/resume`, `/worktree`,
/// `/model`, `/status` — because they differ in what the cells say, not in how
/// a row is read.
#[derive(Debug, Default, PartialEq)]
pub struct Listing {
    pub rows: Vec<Row>,
}

/// One row: what it says, and the aside after it.
#[derive(Debug, Default, PartialEq)]
pub struct Row {
    /// The first cell names the row and is lined up with the first cells
    /// around it; the rest follow it. A row of one cell is a line of prose and
    /// is said as it is.
    pub cells: Vec<String>,
    /// The aside after the cells, in the voice a note is said in everywhere
    /// else — the separator it is said with is the layout's, not this row's.
    pub note: Option<String>,
}

impl Listing {
    /// One row per set of cells.
    pub fn of(rows: impl IntoIterator<Item = Row>) -> Self {
        Self {
            rows: rows.into_iter().collect(),
        }
    }

    /// A line of prose per string: what an answer says when it is not a table,
    /// and what an error comes as.
    pub fn say(lines: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::of(lines.into_iter().map(|l| Row::new([l.into()])))
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl Row {
    pub fn new(cells: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            cells: cells.into_iter().map(Into::into).collect(),
            note: None,
        }
    }

    /// The same row with an aside after it.
    pub fn noting(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The two ways a listing is built end up in the same shape: the cells a
    // table is made of, and the one cell a sentence is.
    #[test]
    fn a_sentence_is_a_row_of_one() {
        let listing = Listing::say(["nothing to list", "and nothing else"]);
        assert_eq!(
            listing,
            Listing::of([
                Row::new(["nothing to list"]),
                Row::new(["and nothing else"])
            ])
        );
        assert!(Listing::default().is_empty());
    }
}
