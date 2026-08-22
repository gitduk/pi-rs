use std::io::{IsTerminal, Write};

use agent::Event;

const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

pub struct Renderer {
    color: bool,
    quiet: bool,
    thinking: bool,
    /// Each stream is tracked separately: they share a terminal when both are
    /// a tty, but only the dirty one may be terminated when piped apart.
    out_dirty: bool,
    err_dirty: bool,
}

impl Renderer {
    pub fn new(quiet: bool) -> Self {
        Self {
            color: std::io::stderr().is_terminal(),
            quiet,
            thinking: false,
            out_dirty: false,
            err_dirty: false,
        }
    }

    fn paint(&self, code: &str, body: &str) -> String {
        if self.color {
            format!("{code}{body}{RESET}")
        } else {
            body.to_string()
        }
    }

    /// Answer text goes to stdout so it pipes; everything else is progress and
    /// goes to stderr.
    pub fn on(&mut self, event: Event) {
        match event {
            Event::ReasoningDelta(d) if !self.quiet => {
                if !self.thinking {
                    self.settle_out();
                    eprint!("{}", self.paint(DIM, "thinking "));
                    self.thinking = true;
                }
                eprint!("{}", self.paint(DIM, &d));
                self.err_dirty = true;
                let _ = std::io::stderr().flush();
            }
            Event::TextDelta(d) => {
                self.end_thinking();
                self.settle_err();
                print!("{d}");
                self.out_dirty = !d.ends_with('\n');
                let _ = std::io::stdout().flush();
            }
            Event::ToolStart { name, args, .. } if !self.quiet => {
                self.end_thinking();
                self.settle();
                eprintln!(
                    "{} {name} {}",
                    self.paint(DIM, "→"),
                    self.paint(DIM, &summarize(&args))
                );
            }
            Event::ToolEnd {
                name,
                is_error,
                preview,
                ..
            } if !self.quiet => {
                self.settle();
                let mark = if is_error {
                    self.paint(RED, "✗")
                } else {
                    self.paint(GREEN, "✓")
                };
                eprintln!("{mark} {name} {}", self.paint(DIM, &clip(&preview, 100)));
            }
            Event::Compacted(r) if !self.quiet => {
                self.end_thinking();
                self.settle();
                eprintln!("{}", self.paint(DIM, &compaction_line(&r)));
            }
            Event::ToolDenied { name, reason, .. } => {
                self.settle();
                eprintln!(
                    "{} {name} {}",
                    self.paint(RED, "✗"),
                    self.paint(DIM, &clip(&reason, 100))
                );
            }
            Event::Done { turns, usage, cost } if !self.quiet => {
                self.end_thinking();
                self.settle();
                eprintln!(
                    "{}",
                    self.paint(
                        DIM,
                        &format!(
                            "{turns} turns · {} in / {} out · {} cached{}",
                            usage.input,
                            usage.output,
                            usage.cache_read,
                            // An unpriced model reports no cost rather than $0.
                            if cost > 0.0 {
                                format!(" · ${cost:.4}")
                            } else {
                                String::new()
                            },
                        )
                    )
                );
            }
            _ => {}
        }
    }

    fn end_thinking(&mut self) {
        self.thinking = false;
    }

    /// Terminate the answer stream's partial line. Never called between two
    /// text deltas: they continue one line, they do not each start one.
    fn settle_out(&mut self) {
        if self.out_dirty {
            println!();
            self.out_dirty = false;
        }
    }

    fn settle_err(&mut self) {
        if self.err_dirty {
            eprintln!();
            self.err_dirty = false;
        }
    }

    /// Before a whole-line write, which must start at column zero on both.
    fn settle(&mut self) {
        self.settle_out();
        self.settle_err();
    }

    pub fn finish(&mut self) {
        self.end_thinking();
        self.settle();
    }
}

/// Says what was given up, not just how much. A silent shrink looks like the
/// agent forgetting things for no reason.
fn compaction_line(r: &agent::compact::Report) -> String {
    let mut parts = Vec::new();
    if r.superseded > 0 {
        parts.push(format!("{} superseded", r.superseded));
    }
    if r.uneventful > 0 {
        parts.push(format!("{} uneventful", r.uneventful));
    }
    if r.aged_out > 0 {
        parts.push(format!("{} aged out", r.aged_out));
    }
    if r.dropped > 0 {
        let how = if r.summarized {
            "summarized"
        } else {
            "dropped"
        };
        parts.push(format!("{} messages {how}", r.dropped));
    }
    let detail = if parts.is_empty() {
        String::new()
    } else {
        format!(" · {}", parts.join(", "))
    };
    let warn = if r.still_over {
        " · still over budget"
    } else {
        ""
    };
    format!("compacted {} → {} tokens{detail}{warn}", r.before, r.after)
}

fn clip(s: &str, max: usize) -> String {
    let one = s.replace('\n', " ");
    match one.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &one[..i]),
        None => one,
    }
}

/// The one argument worth showing in a progress line.
fn summarize(args: &serde_json::Value) -> String {
    // A patch is many lines; the files it touches are the useful part.
    if let Some(patch) = args.get("patch").and_then(|v| v.as_str()) {
        let files: Vec<&str> = patch
            .lines()
            .filter_map(|l| {
                l.trim()
                    .strip_prefix('[')?
                    .strip_suffix(']')?
                    .rsplit_once('#')
            })
            .map(|(path, _)| path)
            .collect();
        return clip(&files.join(" "), 80);
    }
    // `pattern` before `path`: a grep call carries both, and the pattern is the
    // half that says what the agent was looking for.
    for key in ["pattern", "command", "path", "query"] {
        if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
            return clip(v, 80);
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::summarize;
    use serde_json::json;

    #[test]
    fn a_patch_summarizes_to_the_files_it_touches() {
        let patch = "[a.rs#A1B2]\nPUT 1.=1:\n+x\n[b.rs#C3D4]\nREM\n";
        assert_eq!(summarize(&json!({ "patch": patch })), "a.rs b.rs");
    }

    #[test]
    fn consecutive_text_deltas_stay_on_one_line() {
        let mut r = super::Renderer::new(false);
        r.on(agent::Event::TextDelta("There".into()));
        assert!(r.out_dirty, "an unterminated delta leaves the line open");
        r.on(agent::Event::TextDelta("'s a bug".into()));
        // settle_out must not fire between deltas, or every token gets its own line.
        assert!(r.out_dirty);
        r.on(agent::Event::TextDelta("done\n".into()));
        assert!(!r.out_dirty, "a delta ending in a newline closes the line");
    }

    #[test]
    fn other_tools_show_their_leading_argument() {
        assert_eq!(summarize(&json!({ "path": "src/a.rs" })), "src/a.rs");
        assert_eq!(summarize(&json!({ "command": "cargo test" })), "cargo test");
        assert_eq!(summarize(&json!({ "nothing": 1 })), "");
    }

    #[test]
    fn a_compaction_line_names_what_was_given_up() {
        let r = agent::compact::Report {
            before: 130_000,
            after: 48_000,
            superseded: 3,
            uneventful: 1,
            aged_out: 6,
            dropped: 0,
            summarized: false,
            still_over: false,
        };
        assert_eq!(
            super::compaction_line(&r),
            "compacted 130000 → 48000 tokens · 3 superseded, 1 uneventful, 6 aged out"
        );
    }

    #[test]
    fn a_summarized_drop_says_so_rather_than_reading_as_a_loss() {
        let r = agent::compact::Report {
            before: 9,
            after: 5,
            dropped: 4,
            summarized: true,
            ..Default::default()
        };
        assert!(super::compaction_line(&r).contains("4 messages summarized"));
    }

    #[test]
    fn a_compaction_that_did_not_fit_says_so() {
        let r = agent::compact::Report {
            before: 9,
            after: 9,
            still_over: true,
            ..Default::default()
        };
        assert!(super::compaction_line(&r).ends_with("still over budget"));
    }

    #[test]
    fn a_search_shows_what_it_looked_for_not_where() {
        let args = json!({ "pattern": "fn tier", "path": "crates/tools/src" });
        assert_eq!(summarize(&args), "fn tier");
    }
}
