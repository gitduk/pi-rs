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
    for key in ["path", "command", "pattern", "query"] {
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
}
