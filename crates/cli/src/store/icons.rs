//! Every glyph the surfaces draw, in one place. Change a shape here and
//! every surface that wears it follows — a symbol copied through six
//! modules reads as six different things and drifts on the first edit.
//!
//! The two prompt sigils are shapes too; their defaults live here. They are
//! additionally config, so a user's `settings.toml` may override either.
//!
//! A name carries the surface that wears the glyph: `MENU_SIGIL` is the
//! menu's selected row, `CURRENT_ITEM` the list's current row — a change
//! reads as the place it lands.

// The bar the prompt wears, on the input line and on the rows it lands as:
// one shape, so what is being typed and what was said wear the same mark.
pub const PROMPT_BAR: &str = "┃";

/// The bar as every line that wears one writes it: the glyph and the column
/// of air after it — written once, so the input line and the lines it lands
/// as put their first character in the same place.
pub fn bar(icon: &str) -> String {
    format!("{icon} ")
}

// Rows of the scrollback.
pub const SAID_RULE: &str = PROMPT_BAR; // the rule beside a line the user said
// The version line that opens the scrollback.
pub const VERSION_BANNER: &str = concat!("π ", env!("CARGO_PKG_VERSION"));
pub const PENDING_MARK: &str = "→"; // a tool call whose result has not landed
pub const DONE_MARK: &str = "✓"; // a tool row that finished well
pub const FAIL_MARK: &str = "✗"; // a failure, a denial, a refused edit
pub const WARN_MARK: &str = "!"; // a warning line
pub const UNOPENED_MARK: &str = "○"; // a checkout on the bar that no lane has open
pub const COMPACT_RULE: &str = "───"; // the dashes a compaction banner wears

// Menus and lists.
pub const MENU_SIGIL: &str = "›"; // the menu's selected row
pub const CURRENT_ITEM: &str = "●"; // the current row in `/model` and `/resume`
pub const CHANGED_MARK: &str = "≠"; // a session value the file no longer holds

// The input lines.
pub const PIPE_SIGIL: &str = ""; // the prompt where there is no tui
pub const INPUT_SIGIL: &str = PROMPT_BAR; // `theme.prompt.icon` default
pub const INPUT_SIGIL_NORMAL: &str = PROMPT_BAR; // `theme.prompt.normal` default
pub const BANG_SIGIL: &str = "!"; // the `!command` input prompt

// Truncation and joins.
pub const ELLIPSIS: &str = "…"; // a folded or clipped run
pub const PART_SEP: &str = " · "; // between the parts of one line
pub const KEY_NOTE_SEP: &str = "  ·  "; // between a binding's keys and its note

// The spinner.
pub const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const SPIN_STOPPED: &str = "·"; // the static frame a stop lands on

// The WeChat surface.
pub const TOOL_GEAR: &str = "⚙"; // the tool line
pub const RETRY_ARROW: &str = "↻"; // a retry
