//! One row of scrollback, and the only place one can be made.
//!
//! Two producers fill the scrollback and always will: the live stream, which
//! draws content the session does not hold yet, and a rebuild from the
//! transcript, which is the only way back after a rewind. What can be stopped
//! is the second half of that — the same kind of row having two ways to be
//! built. Four of those drifted before this module existed, each found by
//! someone noticing the screen looked different after `/resume` than it had a
//! minute earlier.
//!
//! So `Kind` is private. A row is made by calling one of these constructors,
//! or it is not made at all, and the two callers cannot disagree about what a
//! row of a given kind looks like — there is only one of each.

use std::borrow::Cow;
use std::cell::RefCell;

use brain::message::{ToolResult, ToolResultContent};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::icons;
use crate::render::{self, Paint};
use crate::status::{self, Segment, Snapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldedTool {
    pub name: String,
    pub preview: String,
}

impl FoldedTool {
    pub fn desc(&self) -> String {
        let head = self.preview.lines().next().unwrap_or("").trim();
        if head.is_empty() {
            self.name.clone()
        } else if head.starts_with(&self.name)
            && head[self.name.len()..].starts_with(char::is_whitespace)
        {
            head.to_string()
        } else {
            format!("{} {head}", self.name)
        }
    }
}

pub struct Row(Kind);

// The rendered rows of a tools summary, keyed by what they were painted at:
// width, and the running/spinner frame that decides the leading mark.
// Running rows must replace their frame as the spinner advances, which is
// why the key is more than the width alone.
type PaintedRows = Option<((usize, bool, usize), Vec<String>)>;

enum Kind {
    // One logical line of a prompt the user said: the border and the body
    // kept apart, so wrapping can repeat the border on every screen row the
    // body spans. A single border in the text would be cut at the first
    // wrap — the bar would end mid-air and the rest of the line would run
    // flush against the left edge.
    Said {
        // The painted rule and its column: `SAID_RULE` in the prompt colour.
        border: String,
        // The line's text, painted in the input style, without the border.
        body: String,
    },
    // A painted line the screen alone knows about: the banner, a command's
    // output, a warning. Colour does not depend on width, so painting it
    // early costs nothing.
    //
    // `times` counts the same notice landing again with nothing between it
    // and the last one: a key held down, or a refusal repeated. It renders as
    // one row with a count rather than as a column of identical lines.
    Notice {
        text: String,
        times: usize,
    },
    // A tool's result, kept as its parts. Clipping waits for the frame that
    // needs it: a row clipped at the width it landed at can never grow back
    // when the window does.
    //
    // `painted` holds the last frame's clipping, keyed by the width it was
    // done at. The screen asks for one row at a time, and a result is as many
    // rows as its diff has lines, so painting the whole result per row asked
    // for made a redraw quadratic in the size of the diff — on every spinner
    // tick, for as long as it stayed on screen.
    Result {
        ok: bool,
        name: String,
        preview: String,
        // The row count, so expandability and toggle don't re-walk the
        // preview string on every mouse move.
        preview_lines: usize,
        expanded: bool,
        hovered: bool,
        painted: RefCell<Option<(usize, Vec<String>)>>,
    },
    // What a finished run left behind, kept as its numbers rather than as the
    // string they render to. The segments the config asks for and the theme
    // they are painted in both outlive the run, and a string frozen when it
    // ended answers to neither.
    Tally(Snapshot),
    // A block of reasoning that can be folded or unfolded.
    Reasoning {
        // Which block this row belongs to; the stream appends completed lines
        // to the open block's row and nothing else.
        block: u64,
        lines: Vec<String>,
        folded: bool,
    },
    // A bundle of read-only tool results folded together into a summary row.
    ToolsSummary {
        tools: Vec<FoldedTool>,
        folded: bool,
        running: bool,
        hovered: bool,
        spinner: usize,
        painted: RefCell<PaintedRows>,
    },
}

impl Row {
    /// Something only the screen ever knew. Free-form on purpose — no session
    /// entry answers for it, so nothing can drift.
    pub fn notice(line: impl Into<String>) -> Self {
        Row(Kind::Notice {
            text: line.into(),
            times: 1,
        })
    }

    /// Fold a repeat into this row, if it is the same notice: the scrollback
    /// then shows `line ×2` where a second row would have gone.
    ///
    /// Compares the painted text, which is what makes it safe: two notices
    /// that read alike but are painted differently are different rows, and a
    /// row of any other kind never folds.
    pub fn repeated(&mut self, line: &str) -> bool {
        match &mut self.0 {
            Kind::Notice { text, times } if text == line => {
                *times += 1;
                true
            }
            _ => false,
        }
    }

    /// One reasoning line. Also a string — a reasoning line lives inside a
    /// block's row, not beside it.
    pub fn reasoning_line(line: &str, paint: &Paint) -> String {
        paint.on(&paint.theme.muted, line)
    }

    /// A whole assistant text block, for a caller that has one.
    pub fn answer(text: &str, paint: &Paint) -> Vec<Self> {
        render::render_markdown(text, paint)
            .into_iter()
            .map(Self::notice)
            .collect()
    }

    /// The line a finished run ends on.
    pub fn tally(snap: Snapshot) -> Self {
        Row(Kind::Tally(snap))
    }

    /// A bundle of read-only tool results folded together into a summary row.
    pub fn tools_summary(tools: Vec<FoldedTool>) -> Self {
        Row(Kind::ToolsSummary {
            tools,
            folded: true,
            running: false,
            hovered: false,
            spinner: 0,
            painted: RefCell::new(None),
        })
    }

    /// Add a tool to a tools summary row, if it is one.
    pub fn push_tool(&mut self, name: String, preview: String) -> bool {
        if let Kind::ToolsSummary { tools, painted, .. } = &mut self.0 {
            tools.push(FoldedTool { name, preview });
            *painted.borrow_mut() = None;
            true
        } else {
            false
        }
    }

    /// Whether this row is a folded tools summary row.
    pub fn is_tools_summary(&self) -> bool {
        matches!(&self.0, Kind::ToolsSummary { .. })
    }

    /// Whether this row is an expandable row.
    pub fn is_expandable(&self) -> bool {
        match &self.0 {
            Kind::ToolsSummary { .. } => true,
            Kind::Result { preview_lines, .. } => *preview_lines > render::SKETCHED_ROWS,
            _ => false,
        }
    }

    /// Toggle expand state, returning true if toggled.
    pub fn toggle_expand(&mut self) -> bool {
        match &mut self.0 {
            Kind::ToolsSummary {
                folded, painted, ..
            } => {
                *folded = !*folded;
                *painted.borrow_mut() = None;
                true
            }
            Kind::Result {
                preview_lines,
                expanded,
                painted,
                ..
            } if *preview_lines > render::SKETCHED_ROWS => {
                *expanded = !*expanded;
                *painted.borrow_mut() = None;
                true
            }
            _ => false,
        }
    }

    /// Update running, hovered, and spinner state for expandable rows.
    pub fn update_hover_state(&mut self, running: bool, hovered: bool, spin: usize) {
        match &mut self.0 {
            Kind::ToolsSummary {
                running: r,
                hovered: h,
                spinner: s,
                painted,
                ..
            } => {
                // The rendered rows are keyed by (width, running, spinner),
                // so running frames replace themselves; hovered is the only
                // change that needs the cache dropped by hand.
                let hover_changed = *h != hovered;
                if *r != running || *h != hovered || *r && *s != spin {
                    *r = running;
                    *h = hovered;
                    *s = spin;
                    if hover_changed {
                        *painted.borrow_mut() = None;
                    }
                }
            }
            Kind::Result {
                hovered: h,
                preview_lines,
                painted,
                ..
            } if *preview_lines > render::SKETCHED_ROWS && *h != hovered => {
                *h = hovered;
                *painted.borrow_mut() = None;
            }
            _ => {}
        }
    }

    /// A reasoning block's first row. Later lines go in through `push_line`.
    pub fn reasoning(block: u64, lines: Vec<String>, folded: bool) -> Self {
        Row(Kind::Reasoning {
            block,
            lines,
            folded,
        })
    }

    /// A tool result the screen already has in parts — the live path, which
    /// never holds a `ToolResult`.
    pub fn result(ok: bool, name: impl Into<String>, preview: impl Into<String>) -> Self {
        let preview = preview.into();
        let preview_lines = preview.lines().count();
        Row(Kind::Result {
            ok,
            name: name.into(),
            preview,
            preview_lines,
            expanded: false,
            hovered: false,
            painted: RefCell::new(None),
        })
    }

    /// The same row out of a stored result: the tool's own sketch when it made
    /// one — that is what the stored content does not hold — and otherwise the
    /// first line of that content, which is what `ToolOutput::preview` falls
    /// back to.
    pub fn stored_result(r: &ToolResult, preview: Option<&str>) -> Self {
        let preview = preview.map(str::to_string).unwrap_or_else(|| {
            let body: String = r
                .content
                .iter()
                .filter_map(|c| match c {
                    ToolResultContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect();
            match body.split_once('\n') {
                Some((h, _)) => h.to_string(),
                None => body,
            }
        });
        Self::result(!r.is_error, r.name.clone(), preview)
    }

    /// The tool's name if this row is a tool result.
    pub fn tool_name(&self) -> Option<&str> {
        match &self.0 {
            Kind::Result { name, .. } => Some(name),
            _ => None,
        }
    }

    /// The tool's preview if this row is a tool result.
    pub fn tool_preview(&self) -> Option<&str> {
        match &self.0 {
            Kind::Result { preview, .. } => Some(preview),
            _ => None,
        }
    }

    /// Whether this tool result succeeded.
    pub fn ok(&self) -> Option<bool> {
        match &self.0 {
            Kind::Result { ok, .. } => Some(*ok),
            _ => None,
        }
    }

    /// A tool's start line: what an unanswered call keeps in the view.
    pub fn tool_start(name: &str, summary: &str, paint: &Paint) -> Self {
        Self::notice(paint.on(&paint.theme.muted, &tool_start_line(name, summary)))
    }

    // One logical line of a prompt the user said: the rule it wears, and the
    // text under it. The border lives apart from the body so the screen can
    // repeat it on every row the body wraps to — see `Kind::Said`.
    fn said(border: String, body: String) -> Self {
        Row(Kind::Said { border, body })
    }

    /// A prompt's lines as the stream echoed them: a `!` keeps its own mark
    /// and its continuation the plain indent, everything else wears the rule,
    /// unbroken down every line of it.
    pub fn prompt(text: &str, bang: &str, paint: &Paint) -> Vec<Self> {
        if text.starts_with('!') {
            // A `!` is a command, not something said: the bang takes the
            // prompt's place, and the lines under it keep the plain indent.
            let mut rows = Vec::new();
            for (i, line) in text.lines().enumerate() {
                let (prefix, body) = if i == 0 {
                    (bang, line.strip_prefix('!').unwrap_or(line).trim_start())
                } else {
                    ("  ", line)
                };
                let body = paint.on(&paint.theme.input, body);
                rows.push(Self::notice(format!("{prefix}{body}")));
            }
            return rows;
        }
        // Unbroken down every line said: the icon marks what is being typed,
        // and a landed line wearing it reads as another place to type. The
        // border is kept apart from the body so wrapping can repeat it.
        let border = format!("{} ", paint.on(&paint.theme.prompt.color, icons::SAID_RULE));
        text.lines()
            .map(|line| Self::said(border.clone(), paint.on(&paint.theme.input, line)))
            .collect()
    }

    /// How many screen rows this renders to.
    pub fn len(&self) -> usize {
        match &self.0 {
            Kind::Notice { .. } | Kind::Said { .. } | Kind::Tally(_) => 1,
            Kind::Result {
                preview_lines,
                expanded,
                ..
            } => {
                if *preview_lines <= render::SKETCHED_ROWS {
                    (*preview_lines).max(1)
                } else if *expanded {
                    *preview_lines + 1
                } else {
                    render::SKETCHED_ROWS + 1
                }
            }
            Kind::Reasoning { lines, folded, .. } => {
                if *folded {
                    1
                } else {
                    lines.len()
                }
            }
            Kind::ToolsSummary { tools, folded, .. } => {
                if *folded {
                    1
                } else {
                    1 + tools.len()
                }
            }
        }
    }

    /// What the screen renders for row `i` of this row, at this width: the
    /// text, and the border its continuation rows must repeat. A said row
    /// keeps the rule apart from the body so a line wider than the terminal
    /// can carry it to every row it wraps to; anything else is a single text
    /// with no border to keep.
    pub fn line<'a>(
        &'a self,
        i: usize,
        paint: &'a Paint,
        done: &[Segment],
        width: usize,
    ) -> (Cow<'a, str>, Option<&'a str>) {
        match &self.0 {
            Kind::Said { border, body } => (Cow::Borrowed(body), Some(border)),
            Kind::Notice { text, times } if *times == 1 => (Cow::Borrowed(text), None),
            Kind::Notice { text, times } => {
                // The count wears the muted style whatever the line it trails,
                // so a repeated warning still reads as one warning and a tally.
                let count = paint.on(&paint.theme.muted, &format!(" ×{times}"));
                (Cow::Owned(format!("{text}{count}")), None)
            }
            Kind::Tally(snap) => (
                Cow::Owned(paint.on(&paint.theme.muted, &status::line(done, snap))),
                None,
            ),
            Kind::Result {
                ok,
                name,
                preview,
                expanded,
                hovered,
                painted,
                ..
            } => {
                let mut painted = painted.borrow_mut();
                let rows = match &mut *painted {
                    Some((w, rows)) if *w == width => rows,
                    slot => {
                        let rows = render::result_rows(
                            !*ok, name, preview, *expanded, *hovered, paint, width,
                        );
                        &mut slot.insert((width, rows)).1
                    }
                };
                let text = rows.get(i).cloned().unwrap_or_default();
                (Cow::Owned(text), None)
            }
            Kind::Reasoning { lines, folded, .. } => {
                let text = if *folded {
                    // The count row is synthesized at draw time, so it takes
                    // its muted styling here rather than from a painted row.
                    Cow::Owned(paint.on(&paint.theme.muted, &thinking_summary(lines.len())))
                } else {
                    Cow::Borrowed(lines[i].as_str())
                };
                (text, None)
            }
            Kind::ToolsSummary {
                tools,
                folded,
                running,
                hovered,
                spinner,
                painted,
            } => {
                let mut painted = painted.borrow_mut();
                let rows = match &mut *painted {
                    Some((key, rows)) if *key == (width, *running, *spinner) => rows,
                    slot => {
                        let rows = tools_summary_rows(
                            tools, *folded, *running, *hovered, *spinner, paint, width,
                        );
                        &mut slot.insert(((width, *running, *spinner), rows)).1
                    }
                };
                let text = rows.get(i).cloned().unwrap_or_default();
                (Cow::Owned(text), None)
            }
        }
    }

    /// The reasoning block this row belongs to, if it is one.
    pub fn block(&self) -> Option<u64> {
        match &self.0 {
            Kind::Reasoning { block, .. } => Some(*block),
            _ => None,
        }
    }

    /// Whether this row is folded, and the handle to change it.
    pub fn folded(&self) -> Option<bool> {
        match &self.0 {
            Kind::Reasoning { folded, .. } => Some(*folded),
            _ => None,
        }
    }

    pub fn set_folded(&mut self, to: bool) {
        if let Kind::Reasoning { folded, .. } = &mut self.0 {
            *folded = to;
        }
    }

    /// Whether this row is an empty reasoning block.
    pub fn is_empty_reasoning(&self) -> bool {
        match &self.0 {
            Kind::Reasoning { lines, .. } => lines.is_empty(),
            _ => false,
        }
    }

    /// Append a finished line to a reasoning block. A no-op on anything else,
    /// which no caller can reach: the only handle to a row is one found by
    /// `block()`, and only a reasoning row answers that.
    pub fn push_line(&mut self, painted: String) {
        if let Kind::Reasoning { lines, .. } = &mut self.0 {
            lines.push(painted);
        }
    }

    /// What the screen opens with: the version, and the instruction files this
    /// run stands on.
    ///
    /// Built rather than stored, so a `/reload` onto a new theme replaces the
    /// rows instead of repainting the strings inside them — there is no way to
    /// reach those. The files are shown here rather than said as a startup
    /// note: they are what the run is standing on, not news, and a note about
    /// them scrolls away while this stays at the top where it belongs.
    pub fn banner(context: &[String], paint: &Paint) -> Vec<Self> {
        let muted = |line: &str| Self::notice(paint.on(&paint.theme.muted, line));
        let mut rows = vec![muted(icons::VERSION_BANNER)];
        if !context.is_empty() {
            rows.push(muted("context:"));
            rows.extend(context.iter().map(|f| muted(&format!("- {f}"))));
        }
        rows
    }
}

/// A tool call named the way every row that shows one names it: the tool, and
/// its leading argument when it has one. The prefix is the caller's — a spinner
/// while it runs, an arrow once it is abandoned — and that is the only part
/// that differs.
pub fn named(name: &str, summary: &str) -> String {
    if summary.is_empty() {
        name.to_string()
    } else {
        format!("{name} {summary}")
    }
}

// A tool's start line: what an unanswered call keeps in the view.
fn tool_start_line(name: &str, summary: &str) -> String {
    format!("{} {}", icons::PENDING_MARK, named(name, summary))
}

// The count line a shut thinking block leaves in the scrollback.
fn thinking_summary(n: usize) -> String {
    let s = if n == 1 { "" } else { "s" };
    format!("thinking{}{n} line{s}", icons::PART_SEP)
}

fn clip_to(s: &str, max_cols: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > max_cols {
            return s[..i].trim_end();
        }
        used += w;
    }
    s.trim_end()
}

pub fn tools_summary_header(
    tools: &[FoldedTool],
    folded: bool,
    running: bool,
    hovered: bool,
    spinner: usize,
    paint: &Paint,
    width: usize,
) -> String {
    // A finished batch wears a green check, an in-flight one spins, and an
    // unfolded row shows its expand mark — hover never animates the mark.
    let mark = if !folded {
        icons::UNFOLD_MARK.to_string()
    } else if running {
        icons::SPINNER_FRAMES[spinner % icons::SPINNER_FRAMES.len()].to_string()
    } else {
        paint.on(&paint.theme.status.ok, icons::DONE_MARK)
    };

    let prefix = format!("{mark} Ran ");
    if tools.is_empty() {
        let text = format!("{prefix}0 tools");
        return paint.on(paint.hover_style(hovered), &text);
    }

    let count = tools.len();
    let s = if count == 1 { "" } else { "s" };
    let count_suffix = format!("{count} tool{s}");
    let desc = tools[count - 1].desc();
    let prefix_w = UnicodeWidthStr::width(prefix.as_str());
    let count_w = UnicodeWidthStr::width(count_suffix.as_str());
    // The ellipsis with a space each side, plus a column of air before the
    // terminal edge clips the line.
    let sep_w = 1 + UnicodeWidthStr::width(icons::ELLIPSIS) + 1 + 1;
    let fixed_w = prefix_w + count_w + sep_w;
    let room = width.saturating_sub(fixed_w);
    let budget = room.clamp(8, 50);
    let desc_w = UnicodeWidthStr::width(desc.as_str());
    let text = if desc_w > budget {
        let cut = clip_to(&desc, budget);
        format!("{prefix}{cut} {} {count_suffix}", icons::ELLIPSIS)
    } else {
        format!("{prefix}{desc} {} {count_suffix}", icons::ELLIPSIS)
    };
    paint.on(paint.hover_style(hovered), &text)
}

pub fn tools_summary_rows(
    tools: &[FoldedTool],
    folded: bool,
    running: bool,
    hovered: bool,
    spinner: usize,
    paint: &Paint,
    width: usize,
) -> Vec<String> {
    let header = tools_summary_header(tools, folded, running, hovered, spinner, paint, width);
    if folded {
        return vec![header];
    }
    let mut rows = Vec::with_capacity(1 + tools.len());
    rows.push(header);
    let mark = paint.on(&paint.theme.status.ok, icons::DONE_MARK);
    let room = width.saturating_sub(4).max(10);
    for tool in tools {
        let desc = tool.desc();
        let clipped = clip_to(&desc, room);
        rows.push(format!(
            "  {mark} {}",
            paint.on(&paint.theme.muted, clipped)
        ));
    }
    rows
}

/// Format a folded tool summary line, e.g. "▶ Ran read a.rs ... 18 tools".
#[cfg(test)]
pub fn tools_summary_line(tools: &[FoldedTool], paint: &Paint, width: usize) -> String {
    tools_summary_header(tools, true, false, false, 0, paint, width)
}

#[cfg(test)]
mod said_tests {
    use super::*;
    use crate::icons;

    fn said(text: &str) -> Vec<String> {
        let paint = Paint::new(true);
        Row::prompt(text, "! ", &paint)
            .iter()
            .map(|r| {
                let (body, border) = r.line(0, &paint, &[], 80);
                crate::render::strip_ansi(&format!("{}{}", border.unwrap_or_default(), body))
            })
            .collect()
    }

    // A line that has landed wears a rule, not the prompt icon: the icon
    // marks the line being typed, and one above the input read as a second
    // place to type. The rule runs down every line, so a multi-line say is
    // one bar rather than a mark and some indent.
    #[test]
    fn a_said_line_wears_a_rule_and_never_the_prompt_icon() {
        let icon = crate::render::Theme::default().prompt.icon;
        assert_eq!(said("hi"), [format!("{} hi", icons::SAID_RULE)]);
        assert_eq!(
            said("first\nsecond\nthird"),
            [
                format!("{} first", icons::SAID_RULE),
                format!("{} second", icons::SAID_RULE),
                format!("{} third", icons::SAID_RULE),
            ],
            "the rule is unbroken"
        );
        for row in said("hi\nthere") {
            assert!(!row.contains(&icon), "the icon is the input line's: {row}");
        }
    }

    // A `!` is a command, not something said, and keeps its own mark.
    #[test]
    fn a_bang_command_keeps_its_own_mark() {
        assert_eq!(said("!cargo test"), ["! cargo test"]);
    }

    // The rule spends the same two columns the prompt did, so nothing that
    // lines up against a said line moves.
    #[test]
    fn the_rule_costs_what_the_prompt_did() {
        let icon = crate::render::Theme::default().prompt.icon;
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(icons::SAID_RULE),
            unicode_width::UnicodeWidthStr::width(icon.as_str()),
        );
    }
}

#[cfg(test)]
mod tools_summary_tests {
    use super::*;

    fn tool(name: &str, preview: &str) -> FoldedTool {
        FoldedTool {
            name: name.to_string(),
            preview: preview.to_string(),
        }
    }

    #[test]
    fn single_tool_summary() {
        let paint = Paint::new(false);
        let tools = vec![tool("read", "crates/agent/src/session.rs")];
        let line = tools_summary_line(&tools, &paint, 80);
        assert_eq!(
            line,
            format!(
                "{} Ran read crates/agent/src/session.rs {} 1 tool",
                icons::DONE_MARK,
                icons::ELLIPSIS
            )
        );
    }

    #[test]
    fn multiple_tools_shows_latest() {
        let paint = Paint::new(false);
        let mut tools: Vec<FoldedTool> = (0..11)
            .map(|i| tool("read", &format!("file_{i}.rs")))
            .collect();
        tools.push(tool("read", "crates/agent/src/session.rs"));
        let line = tools_summary_line(&tools, &paint, 80);
        assert_eq!(
            line,
            format!(
                "{} Ran read crates/agent/src/session.rs {} 12 tools",
                icons::DONE_MARK,
                icons::ELLIPSIS
            )
        );

        tools.push(tool("read", "/path/to/other.rs"));
        let line = tools_summary_line(&tools, &paint, 80);
        assert_eq!(
            line,
            format!(
                "{} Ran read /path/to/other.rs {} 13 tools",
                icons::DONE_MARK,
                icons::ELLIPSIS
            )
        );
    }

    #[test]
    fn long_tool_call_is_truncated() {
        let paint = Paint::new(false);
        let mut tools: Vec<FoldedTool> = (0..13)
            .map(|i| tool("read", &format!("file_{i}.rs")))
            .collect();
        tools.push(tool(
            "bash",
            "curl https://xxxx.xxxx.com/a/long/url/and/much/more/parameters/and/data",
        ));
        let line = tools_summary_line(&tools, &paint, 80);
        assert!(
            line.contains(&format!(
                "{} Ran bash curl https://xxxx.xxxx.com/a/long/url",
                icons::DONE_MARK
            )),
            "got: {line}"
        );
        assert!(
            line.ends_with(&format!("{} 14 tools", icons::ELLIPSIS)),
            "got: {line}"
        );
    }

    #[test]
    fn empty_preview_falls_back_to_tool_name() {
        let paint = Paint::new(false);
        let tools = vec![tool("bash", "")];
        let line = tools_summary_line(&tools, &paint, 80);
        assert_eq!(
            line,
            format!("{} Ran bash {} 1 tool", icons::DONE_MARK, icons::ELLIPSIS)
        );
    }

    #[test]
    fn multiline_preview_uses_first_line() {
        let paint = Paint::new(false);
        let tools = vec![tool(
            "bash",
            "git status\nnothing to commit\nworking tree clean",
        )];
        let line = tools_summary_line(&tools, &paint, 80);
        assert_eq!(
            line,
            format!(
                "{} Ran bash git status {} 1 tool",
                icons::DONE_MARK,
                icons::ELLIPSIS
            )
        );
    }

    #[test]
    fn running_tool_shows_spinner_animation() {
        let paint = Paint::new(false);
        let tools = vec![tool("read", "a.rs")];
        let frame0 = tools_summary_header(&tools, true, true, false, 0, &paint, 80);
        assert!(frame0.starts_with(&format!("{} Ran", icons::SPINNER_FRAMES[0])));
        let frame1 = tools_summary_header(&tools, true, true, false, 1, &paint, 80);
        assert!(frame1.starts_with(&format!("{} Ran", icons::SPINNER_FRAMES[1])));
    }

    // The rendered rows are cached by (width, running, spinner), so a running
    // summary row must replace its frame as the spinner advances — a cache
    // keyed by width alone would freeze the row on its first frame.
    #[test]
    fn a_running_summary_row_advances_through_the_paint_cache() {
        let paint = Paint::new(true);
        let mut row = Row::tools_summary(vec![tool("read", "a.rs")]);

        row.update_hover_state(true, false, 0);
        let f0 = crate::render::strip_ansi(&row.line(0, &paint, &[], 80).0);
        row.update_hover_state(true, false, 1);
        let f1 = crate::render::strip_ansi(&row.line(0, &paint, &[], 80).0);

        assert!(
            f0.starts_with(&format!("{} Ran", icons::SPINNER_FRAMES[0])),
            "got: {f0}"
        );
        assert!(
            f1.starts_with(&format!("{} Ran", icons::SPINNER_FRAMES[1])),
            "got: {f1}"
        );
        assert_ne!(f0, f1, "the running frame must advance, not freeze");
    }

    #[test]
    fn hovered_tool_stays_on_the_green_check() {
        let paint = Paint::new(true);
        let tools = vec![tool("read", "a.rs")];
        let h0 = tools_summary_header(&tools, true, false, true, 0, &paint, 80);
        let h1 = tools_summary_header(&tools, true, false, true, 1, &paint, 80);
        assert_eq!(h0, h1, "hover must not animate the mark");
        assert!(
            crate::render::strip_ansi(&h0).starts_with(&format!("{} Ran", icons::DONE_MARK)),
            "got: {h0}"
        );
    }

    #[test]
    fn unfolded_tools_summary_shows_all_tools() {
        let paint = Paint::new(false);
        let tools = vec![
            tool("read", "crates/agent/src/session.rs"),
            tool("grep", "match 1"),
        ];
        let mut row = Row::tools_summary(tools);
        assert_eq!(row.len(), 1);

        assert!(row.toggle_expand());
        assert_eq!(row.len(), 3);

        let (head, _) = row.line(0, &paint, &[], 80);
        assert!(head.starts_with(&format!("{} Ran", icons::UNFOLD_MARK)));
        assert!(head.contains("2 tools"));

        let (t0, _) = row.line(1, &paint, &[], 80);
        assert!(t0.contains(&format!(
            "{} read crates/agent/src/session.rs",
            icons::DONE_MARK
        )));

        let (t1, _) = row.line(2, &paint, &[], 80);
        assert!(t1.contains(&format!("{} grep match 1", icons::DONE_MARK)));

        assert!(row.toggle_expand());
        assert_eq!(row.len(), 1);
        let (folded_head, _) = row.line(0, &paint, &[], 80);
        assert!(folded_head.starts_with(&format!("{} Ran", icons::DONE_MARK)));
    }

    #[test]
    fn result_with_many_diff_lines_expands_and_collapses() {
        let paint = Paint::new(false);
        let mut preview = "crates/foo.rs +30 -0".to_string();
        for i in 1..=30 {
            preview.push_str(&format!("\n  {i} + line {i}"));
        }
        let mut row = Row::result(true, "edit", preview);
        assert_eq!(row.len(), 26);
        let (last_folded, _) = row.line(25, &paint, &[], 80);
        assert!(last_folded.contains(&format!("{} 6 more", icons::ELLIPSIS)));

        assert!(row.toggle_expand());
        assert_eq!(row.len(), 32);
        let (last_expanded, _) = row.line(31, &paint, &[], 80);
        assert!(last_expanded.contains("collapse"));

        assert!(row.toggle_expand());
        assert_eq!(row.len(), 26);
    }
}
