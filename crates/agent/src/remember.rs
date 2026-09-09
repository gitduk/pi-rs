//! Asking the model what should outlive the span being dropped.
//!
//! Beside `summarize`, not inside it. The summary carries the work forward
//! within this session; this carries a few facts past it. One call each, run
//! together, because folding two judgements into one prompt gets both done
//! worse.

use brain::model::ModelSpec;
use brain::stream::Usage;
use brain::transport::Transport;

pub const PROMPT: &str = include_str!("../prompts/remember.md");

// Enough for a handful of one-line notes and not enough for prose.
const MAX_TOKENS: u32 = 600;

// More than the prompt asks for, so a model that overruns is trimmed rather
// than thrown away whole.
const MAX_NOTES: usize = 8;

/// One thing worth keeping, and what the model thought it was worth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub text: String,
    /// 1 to 3. A line carrying anything else was not a note.
    pub weight: u8,
}

/// Where kept notes go. What file that is, and how it ages, belongs to
/// whoever built the agent — this crate knows nothing of `~/.pi`.
pub trait Shelf: Send + Sync {
    // What is already kept, rendered as the model should read it. Sent with
    // the request so the same fact is not written down twice in new words.
    fn read(&self) -> Option<String>;

    // Put these on the shelf. Failing is not fatal: losing a note costs a
    // fact, failing the compaction costs the run.
    fn keep(&self, notes: Vec<Kept>);
}

/// A shelf that can be read and not written.
///
/// What a subagent gets. It has no indicator, no place on the lane bar, and
/// the user may not know it was sent — so a note it left would be a fact
/// nobody watched arrive. It works from the same ones all the same.
pub struct ReadOnly(pub std::sync::Arc<dyn Shelf>);

impl Shelf for ReadOnly {
    fn read(&self) -> Option<String> {
        self.0.read()
    }

    fn keep(&self, _: Vec<Kept>) {}
}

/// Read the model's answer, one note to a line. Anything that is not
/// `<weight> <text>` is dropped: a preamble kept as a note is read next month
/// as a fact.
pub fn parse(text: &str) -> Vec<Kept> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches(['-', '*', '•']).trim();
            let (weight, rest) = line.split_once(' ')?;
            let weight: u8 = weight.trim_end_matches(['.', ')']).parse().ok()?;
            let text = rest.trim();
            (!text.is_empty() && (1..=3).contains(&weight)).then(|| Kept {
                text: text.to_string(),
                weight,
            })
        })
        .take(MAX_NOTES)
        .collect()
}

/// Ask what from `history` should outlive the session.
///
/// `focus` is the same hint `/compact` gave the summary: at the moment the
/// user says what matters, they are answering this question too.
pub async fn run(
    transport: &dyn Transport,
    spec: &ModelSpec,
    history: String,
    focus: Option<&str>,
    kept: Option<String>,
) -> brain::Result<(Vec<Kept>, Usage)> {
    // The history unchanged when nothing goes in front of it: it is the whole
    // dropped span, and copying it to prepend nothing is the copy to avoid.
    let mut prefix = String::new();
    if let Some(kept) = kept.filter(|k| !k.trim().is_empty()) {
        prefix.push_str(&format!("Already on the shelf:\n{kept}\n\n"));
    }
    if let Some(f) = focus.map(str::trim).filter(|f| !f.is_empty()) {
        prefix.push_str(&format!("The user says what matters here is: {f}\n\n"));
    }
    let body = match prefix.is_empty() {
        true => history,
        false => prefix + &history,
    };

    let (text, usage) = crate::oneshot::ask(transport, spec, PROMPT, body, MAX_TOKENS).await?;
    // Nothing worth keeping is a correct answer, and the prompt says so.
    Ok((parse(&text), usage))
}

#[cfg(test)]
mod tests {
    use super::{Kept, parse};

    fn kept(text: &str, weight: u8) -> Kept {
        Kept {
            text: text.into(),
            weight,
        }
    }

    #[test]
    fn a_weight_and_a_line_make_a_note() {
        assert_eq!(
            parse("3 prefers xh over curl\n1 the parser is in syntax/\n"),
            vec![
                kept("prefers xh over curl", 3),
                kept("the parser is in syntax/", 1)
            ]
        );
    }

    // A model that wrote prose has said something other than a note, and
    // prose on the shelf is read next month as a fact.
    #[test]
    fn anything_that_is_not_a_note_is_not_kept() {
        let answer = "Here is what I found worth keeping:\n\
                      \n\
                      2 --tools was deleted\n\
                      Let me know if you want more.\n\
                      9 out of range\n\
                      0 also out of range\n\
                      3\n";
        assert_eq!(parse(answer), vec![kept("--tools was deleted", 2)]);
    }

    // The prompt forbids bullets, so a model that adds them has not written a
    // different thing — only a decorated one.
    #[test]
    fn a_bulleted_note_is_still_a_note() {
        assert_eq!(
            parse("- 3 no private-address filter"),
            vec![kept("no private-address filter", 3)]
        );
        assert_eq!(
            parse("* 2. the fence is tier.fenced()"),
            vec![kept("the fence is tier.fenced()", 2)]
        );
    }

    #[test]
    fn nothing_worth_keeping_is_an_answer() {
        assert!(parse("").is_empty());
        assert!(parse("(none)").is_empty());
        assert!(parse("Nothing here should outlive the session.").is_empty());
    }

    #[test]
    fn a_model_that_overruns_is_trimmed_rather_than_dropped() {
        let many: String = (0..20).map(|i| format!("2 note {i}\n")).collect();
        assert_eq!(parse(&many).len(), super::MAX_NOTES);
    }
}
