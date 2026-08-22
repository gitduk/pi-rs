use std::io::{IsTerminal, Write};

use agent::Event;

const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

/// Whether the surface being written to can carry colour.
#[derive(Debug, Clone, Copy)]
pub struct Paint {
    pub color: bool,
}

impl Paint {
    pub fn on(&self, code: &str, body: &str) -> String {
        if self.color {
            format!("{code}{body}{RESET}")
        } else {
            body.to_string()
        }
    }
}

/// The wording for every event that occupies a whole line.
///
/// Both surfaces call this: a tool call has to read the same in a pipe as in
/// the terminal, and two copies of the wording would drift on the first edit.
/// Events that are a fragment rather than a line — the two deltas — are the
/// caller's to place, and return None.
pub fn describe(event: &Event, p: Paint) -> Option<String> {
    Some(match event {
        Event::ToolStart { name, args, .. } => {
            format!("{} {name} {}", p.on(DIM, "→"), p.on(DIM, &summarize(args)))
        }
        Event::ToolEnd {
            name,
            is_error,
            preview,
            ..
        } => {
            let mark = if *is_error {
                p.on(RED, "✗")
            } else {
                p.on(GREEN, "✓")
            };
            format!("{mark} {name} {}", p.on(DIM, &clip(preview, 100)))
        }
        Event::ToolDenied { name, reason, .. } => {
            format!(
                "{} {name} {}",
                p.on(RED, "✗"),
                p.on(DIM, &clip(reason, 100))
            )
        }
        Event::Compacted(r) => p.on(DIM, &compaction_line(r)),
        Event::Retrying {
            attempt,
            delay_ms,
            reason,
        } => p.on(
            DIM,
            &format!("retry {attempt} in {delay_ms}ms · {}", clip(reason, 90)),
        ),
        Event::Warning(w) => format!("{} {}", p.on(RED, "!"), p.on(DIM, w)),
        Event::Done {
            turns,
            usage,
            cost,
            estimated,
        } => {
            // A tilde on every figure, because a count we made is not the
            // provider's and a cost derived from it is not a bill.
            let m = if *estimated { "~" } else { "" };
            p.on(
                DIM,
                &format!(
                    "{turns} turns · {m}{} in / {m}{} out · {} cached{}",
                    usage.input,
                    usage.output,
                    usage.cache_read,
                    // An unpriced model reports no cost rather than $0.
                    if *cost > 0.0 {
                        format!(" · {m}${cost:.4}")
                    } else {
                        String::new()
                    },
                ),
            )
        }
        _ => return None,
    })
}

pub struct Renderer {
    paint: Paint,
    quiet: bool,
    /// A schema was asked for, so stdout belongs to the result alone. Prose the
    /// model produces on the way there is progress, not the answer.
    structured: bool,
    thinking: bool,
    /// Each stream is tracked separately: they share a terminal when both are
    /// a tty, but only the dirty one may be terminated when piped apart.
    out_dirty: bool,
    err_dirty: bool,
}

impl Renderer {
    pub fn new(quiet: bool, structured: bool) -> Self {
        Self {
            paint: Paint {
                color: std::io::stderr().is_terminal(),
            },
            quiet,
            structured,
            thinking: false,
            out_dirty: false,
            err_dirty: false,
        }
    }

    /// Answer text goes to stdout so it pipes; everything else is progress and
    /// goes to stderr.
    pub fn on(&mut self, event: Event) {
        match &event {
            Event::ReasoningDelta(d) if !self.quiet => {
                if !self.thinking {
                    self.settle_out();
                    eprint!("{}", self.paint.on(DIM, "thinking "));
                    self.thinking = true;
                }
                eprint!("{}", self.paint.on(DIM, d));
                self.err_dirty = true;
                let _ = std::io::stderr().flush();
            }
            Event::TextDelta(d) if self.structured => {
                if self.quiet {
                    return;
                }
                self.end_thinking();
                self.settle_out();
                eprint!("{}", self.paint.on(DIM, d));
                self.err_dirty = !d.ends_with('\n');
                let _ = std::io::stderr().flush();
            }
            Event::TextDelta(d) => {
                self.end_thinking();
                self.settle_err();
                print!("{d}");
                self.out_dirty = !d.ends_with('\n');
                let _ = std::io::stdout().flush();
            }
            // Worth seeing even under --quiet: the run did less than it was asked.
            Event::ToolDenied { .. } => {
                self.settle();
                if let Some(line) = describe(&event, self.paint) {
                    eprintln!("{line}");
                }
            }
            _ if self.quiet => {}
            _ => {
                if let Some(line) = describe(&event, self.paint) {
                    self.end_thinking();
                    self.settle();
                    eprintln!("{line}");
                }
            }
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

pub fn clip(s: &str, max: usize) -> String {
    let one = s.replace('\n', " ");
    match one.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &one[..i]),
        None => one,
    }
}

/// The one argument worth showing in a progress line.
pub fn summarize(args: &serde_json::Value) -> String {
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
    fn a_schema_keeps_prose_off_stdout() {
        let mut r = super::Renderer::new(false, true);
        r.on(agent::Event::TextDelta("thinking out loud".into()));
        // stdout carries the result and nothing else, so it pipes into jq.
        assert!(!r.out_dirty, "prose must not reach stdout under a schema");
        assert!(r.err_dirty);
    }

    #[test]
    fn consecutive_text_deltas_stay_on_one_line() {
        let mut r = super::Renderer::new(false, false);
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
