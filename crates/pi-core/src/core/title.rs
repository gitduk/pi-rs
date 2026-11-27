//! The one line a model writes from a session's first question, so a row reads
//! as a subject rather than as a truncated paragraph.

use crate::core::Core;

/// Written under, not asked with: the question is the body, and this is the
/// same string on every session — which is what makes it worth caching.
const PROMPT: &str = "Name the session this opening question began, in at most \
six words, in the language it is written in. Answer with the name alone: no \
quotes, no full stop, no explanation.";

/// A title is a handful of words; past this the model is explaining itself.
const MAX_TOKENS: u32 = 32;

/// How much of the question the model needs to name the session: a pasted file
/// or a stack trace says nothing more about the subject after the first lines.
const ASK: usize = 2_000;

/// A title a model wrote, on its way back to the session it is for.
pub struct Titled {
    pub lane: u64,
    /// The session the question came from: a `/new` or `/resume` in the
    /// meantime leaves this lane holding another one.
    pub session: String,
    pub title: String,
}

impl Core {
    /// Have the summary model name `at` from the first thing it was asked.
    ///
    /// Off the run, never inside it: a title may take a moment to arrive and
    /// none of that moment belongs to the turn, nor may a run in flight wait
    /// for a name it does not use.
    pub(crate) fn start_title(&mut self, at: usize, ask: String) {
        let lane = self.lanes.at(at);
        if !self.naming || lane.name().is_some() || lane.title().is_some() {
            return;
        }
        if ask.trim().is_empty() {
            return;
        }
        let agent = lane.agent().clone();
        let idle = lane.resolved().retry.idle;
        let session = lane.id().to_owned();
        let token = lane.token();
        let titles = self.runtime.titles();
        tokio::spawn(async move {
            let (wire, spec) = agent.side();
            let asked = agent::compaction::ask(
                wire,
                spec,
                PROMPT,
                pi_store::text::clip(&ask, ASK),
                MAX_TOKENS,
                idle,
                Some(&session),
            )
            .await;
            let title = match asked {
                Ok((text, ..)) => for_row(&text),
                Err(error) => {
                    tracing::info!(target: "pi::session", session, %error, "no title this time");
                    None
                }
            };
            if let Some(title) = title {
                let _ = titles.send(Titled {
                    lane: token,
                    session,
                    title,
                });
            }
        });
    }

    /// Put a title on the session it was written for, and file it: `/resume`
    /// reads its row off the file, so the row has to be able to show it now.
    pub(crate) fn land_title(&mut self, titled: Titled) {
        let Some(at) = self.position_of(titled.lane) else {
            return;
        };
        // The lane has to hold the session that asked, and a name the user
        // gave while the model was thinking is the one that stands.
        if self.lanes[at].id() != titled.session || self.lanes[at].name().is_some() {
            return;
        }
        self.lanes.at_mut(at).set_title(Some(titled.title));
        self.save_lane_behind(at);
    }
}

/// A title as a row can take it: one line, no wrapping quotes, within what a
/// list or a completion shows. Nothing to say leaves no title at all.
fn for_row(text: &str) -> Option<String> {
    let one: String = pi_store::text::collapsed(text)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let one = one.trim_matches(|c: char| {
        matches!(
            c,
            '"' | '\'' | '`' | '“' | '”' | '‘' | '’' | '*' | '#' | ':' | '.' | '-' | '—'
        )
    });
    (!one.is_empty()).then(|| pi_store::text::clip(one, pi_store::session::RESUME_WIDTH))
}
