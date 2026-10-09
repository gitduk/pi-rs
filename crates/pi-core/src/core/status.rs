//! What a status line reads: the parts of a lane a reader is shown, and the
//! numbers behind them.
//!
//! The vocabulary is `pi_store::status` and the drawing is `ui/status.rs`; this
//! is where the values come from.

use agent::Totals;
use agent::instructions::short;
use llm::figures;

use super::Core;
use pi_store::icons;
use pi_store::journal;
use pi_store::listing::{Listing, Row};

impl Core {
    /// What `/status` shows: what the lane runs on, what it holds and has
    /// spent, how far it may reach, and where it keeps its records.
    pub(super) fn status(&self) -> Listing {
        let lane = self.lane();
        let resolved = lane.resolved();
        let spec = lane.agent().spec();
        let root = lane.root();
        let sep = icons::PART_SEP;
        let mut rows = Vec::new();

        let effort = lane.agent().brief.effort.name();
        rows.push(Row::new([
            "model".into(),
            format!(
                "{}{sep}effort {effort}{sep}{} window",
                spec.model,
                figures::short(spec.context_window.into())
            ),
        ]));
        // The file's endpoint names its source; a model on its own host
        // names that host instead, since that is where requests go.
        match &resolved.endpoint {
            Some(endpoint)
                if endpoint
                    .strip_prefix("endpoint: ")
                    .and_then(|e| e.split_once(" ("))
                    .is_some_and(|(url, _)| url == spec.base_url) =>
            {
                let endpoint = endpoint.strip_prefix("endpoint: ").unwrap_or(endpoint);
                rows.push(Row::new(["endpoint", endpoint]));
            }
            _ => rows.push(Row::new([
                "endpoint".into(),
                format!("{} ([models.\"{}\"])", spec.base_url, spec.model),
            ])),
        }

        // The last run's own count when there was one; before any, an estimate.
        let held = lane.tally().ctx().or_else(|| {
            let used = llm::estimate::tokens(&lane.session()?.context(), spec);
            let agent = lane.agent();
            Some(agent.occupancy(used, agent.window()))
        });
        if let Some((used, window)) = held.filter(|&(_, w)| w > 0) {
            rows.push(Row::new([
                "context".into(),
                format!(
                    "{} / {} ({}%)",
                    figures::short(used as u64),
                    figures::short(window as u64),
                    used * 100 / window
                ),
            ]));
        }
        // Left out until something has been spent: a session nothing has been
        // asked of yet has no figure, and a row of dashes is not one.
        let spent = lane.tally().session();
        if spent != Totals::default() {
            rows.push(Row::new(["spent".into(), pi_store::text::spent(&spent)]));
        }

        let tier = format!("{:?}", resolved.ceiling).to_lowercase();
        rows.push(Row::new(["tier", &tier]));
        let mut workspace = agent::instructions::home()
            .and_then(|h| Some(format!("~/{}", root.strip_prefix(h).ok()?.display())))
            .unwrap_or_else(|| root.display().to_string());
        if let Some(tree) = lane.worktree() {
            workspace.push_str(&format!("{sep}worktree {tree}"));
        }
        rows.push(Row::new(["workspace".into(), workspace]));
        let extra = lane.workspace().write_roots();
        if tool::Tier::Write.under(resolved.ceiling) && !extra.is_empty() {
            let extra: Vec<_> = extra.iter().map(|p| short(p, root)).collect();
            rows.push(Row::new(["also writes".into(), extra.join(", ")]));
        }
        if let Some(system) = &resolved.system {
            rows.push(Row::new(["system prompt".into(), short(system, root)]));
        }
        if !resolved.instructions.is_empty() {
            rows.push(Row::new([
                "instructions".into(),
                resolved.instructions.join(", "),
            ]));
        }
        rows.push(match toolbox::rtk::state() {
            toolbox::rtk::State::On(version) => Row::new(["rtk".into(), version]),
            toolbox::rtk::State::Off => Row::new(["rtk", "off"]).noting("RTK_DISABLED=1"),
            toolbox::rtk::State::Missing => {
                Row::new(["rtk", "not found"]).noting("commands run as written")
            }
            toolbox::rtk::State::Unknown => Row::new(["rtk", "not asked yet"]),
        });
        let servers = crate::core::mcp::summary();
        if !servers.is_empty() {
            rows.push(Row::new(["mcp".into(), servers.join(", ")]));
        }
        if !resolved.memory.is_empty() {
            rows.push(Row::new(["memory".into(), resolved.memory.join(", ")]));
        }

        rows.push(Row::new([
            "session".into(),
            short(&self.store.path_of(root, lane.id()), root),
        ]));
        rows.push(match journal::path() {
            Some(p) => Row::new(["journal".into(), short(&p, root)]),
            None => Row::new(["journal", "off"]).noting("PI_LOG is off, or it would not open"),
        });
        Listing::of(rows)
    }

    /// What a named lane's transcript occupies now, for the line that says why
    /// there was nothing to compact. Zero while a run has it.
    pub fn tokens_now_at(&self, at: usize) -> usize {
        let Some(lane) = self.lanes.get(at) else {
            return 0;
        };
        lane.session().map_or(0, |s| {
            llm::estimate::tokens(&s.context(), lane.agent().spec())
        })
    }
}

// What becomes of prior-turn reasoning once another model reads it — the
// signed path is out, so it's one of these three.
pub(super) fn demotion(replay: llm::model::ReplayThinking) -> &'static str {
    use llm::model::ReplayThinking as R;
    match replay {
        R::Tagged => "reasoning from the earlier turns replays wrapped in <think> tags",
        R::Off => "reasoning from the earlier turns is dropped rather than replayed",
    }
}

// Whether the transcript holds prior-turn reasoning — worth saying at a
// switch, since it's the one part of history that doesn't survive intact.
pub(super) fn carries_reasoning(session: &agent::session::Session) -> bool {
    // The view, not every entry: what compaction has already dropped is not
    // going to reach the new model in any form, demoted or otherwise.
    session.view().iter().any(|s| {
        s.entry().blocks().is_some_and(|bs| {
            bs.iter()
                .any(|b| matches!(b, llm::message::AssistantContent::Reasoning(_)))
        })
    })
}
