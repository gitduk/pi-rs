//! Every glyph the surface draws, in one place, so a symbol used in several
//! modules stays one thing instead of drifting on the first edit.
//!
//! The two prompt sigils are also config: `settings.toml` may override either.
//! A name carries the surface it draws on (e.g. `MENU_SIGIL`, `CURRENT_ITEM`).

// The bar the prompt wears, on the input line and on the rows it lands as:
// one shape, so what is being typed and what was said wear the same mark.
pub const PROMPT_BAR: &str = "┃";

/// The bar as every line that wears one writes it: the glyph and the column
/// of air after it — written once, so the input line and the lines it lands
/// as put their first character in the same place.
pub fn bar(icon: &str) -> String {
    format!("{icon} ")
}

pub const SAID_RULE: &str = PROMPT_BAR; // the rule beside a line the user said
// The version line that opens the scrollback.
pub const VERSION_BANNER: &str = concat!("π ", env!("CARGO_PKG_VERSION"));
pub const PENDING_MARK: &str = "→"; // a call started, on a progress line that cannot animate
pub const STOPPED_MARK: &str = "✖"; // a call made and never answered: stopped before it landed
// A call in flight: out and back, holding a frame at each end. `*` over
// `✳`, which some fonts draw as an emoji.
pub const CALL_FRAMES: [&str; 12] = ["·", "✢", "*", "✶", "✻", "✽", "✽", "✻", "✶", "*", "✢", "·"];
pub const DONE_MARK: &str = "✔"; // a tool row that finished well
pub const FAIL_MARK: &str = "✖"; // a failure, a denial, a refused edit
pub const WARN_MARK: &str = "!";
pub const UNOPENED_MARK: &str = "○"; // a checkout on the bar that no lane has open
pub const COMPACT_RULE: &str = "───"; // the dashes a compaction banner wears

pub const CURRENT_ITEM: &str = "●"; // the current row in `/model` and `/resume`

pub const INPUT_SIGIL: &str = PROMPT_BAR; // `theme.prompt.icon` default
pub const INPUT_SIGIL_NORMAL: &str = PROMPT_BAR; // `theme.prompt.normal` default
pub const BANG_SIGIL: &str = "!"; // the `!command` input prompt

pub const ELLIPSIS: &str = "…"; // a folded or clipped run
pub const PART_SEP: &str = " · ";
pub const KEY_NOTE_SEP: &str = "  ·  "; // between a binding's keys and its note
