//! The shape of a read-only answer: the rows a command has to show, before
//! anyone lays them out.
//!
//! Lives here because the key table in `keys` fills one in too.

/// A read-only answer, as rows of cells: the layout is decided by whoever
/// draws it, not here.
///
/// A report can give its rows semantic roles so each surface can arrange
/// summaries and sections without knowing the command that produced them.
#[derive(Debug, Default, PartialEq)]
pub struct Listing {
    pub rows: Vec<Row>,
    /// A list read by scanning: one screen row per row, cut where it ends,
    /// rather than wrapped to read whole.
    pub brief: bool,
    /// Reports group semantic rows into independently sized sections.
    pub format: Format,
}

/// The document structure, independent of a particular surface.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    #[default]
    Rows,
    Report,
}

/// A row's meaning in a report; the surface chooses its appearance.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    #[default]
    Plain,
    Title,
    Metric,
    Progress {
        used: usize,
        total: usize,
    },
    Section,
    Budget,
    Hint,
    Footer,
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
    pub role: Role,
}

impl Listing {
    /// One row per set of cells.
    pub fn of(rows: impl IntoIterator<Item = Row>) -> Self {
        Self {
            rows: rows.into_iter().collect(),
            brief: false,
            format: Format::Rows,
        }
    }

    /// A report with a lead summary and independently arranged sections.
    pub fn report(rows: impl IntoIterator<Item = Row>) -> Self {
        Self {
            format: Format::Report,
            ..Self::of(rows)
        }
    }

    /// The same rows, each kept to one screen row.
    pub fn brief(mut self) -> Self {
        self.brief = true;
        self
    }

    /// A line of prose per string: what an answer says when it is not a table,
    /// and what an error comes as.
    pub fn say(lines: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::of(lines.into_iter().map(|l| Row::new([l.into()])))
    }

    /// Whether a surface lays this out as a report: sections it arranges
    /// itself, rather than rows drawn as they come.
    pub fn is_report(&self) -> bool {
        self.format == Format::Report
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
            role: Role::Plain,
        }
    }

    /// Give a report row its semantic role.
    pub fn with_role(mut self, role: Role) -> Self {
        self.role = role;
        self
    }

    /// The same row with an aside after it.
    pub fn noting(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}
