//! What a status line reads: the parts of a lane a reader is shown, and the
//! numbers behind them.
//!
//! The vocabulary is `pi_store::status` and the drawing is the surface's; this
//! is where the values come from.

use crate::core::instructions::short;
use agent::Totals;
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

        let effort = match spec.accepts_effort {
            true => lane.agent().brief.effort.name(),
            false => "unsupported",
        };
        rows.push(Row::new([
            "model".into(),
            format!(
                "{}{sep}effort {effort}{sep}{} window",
                spec.model,
                figures::size(spec.context_window.into())
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
        // Filed under the reply it cut; gathered here for the whole run.
        let tally = lane.tally();
        if !tally.gaps().is_empty() {
            rows.push(Row::new([
                "incomplete reply".into(),
                tally.gaps().join("; "),
            ]));
        }

        // Only the provider's count, which a compaction or a resume voids.
        let context = match lane.tally().ctx().filter(|&(_, w)| w > 0) {
            Some((used, window)) => format!(
                "{} / {} ({}%)",
                figures::short(used as u64),
                figures::size(window as u64),
                used * 100 / window
            ),
            None => "counted by the next request".to_string(),
        };
        rows.push(Row::new(["context".into(), context]));
        // Behind the usage line's `compacted ×N` and `↓`.
        if let Some(report) = tally.compacted() {
            let line = super::meter::compaction_line(report);
            let line = line
                .strip_prefix(&format!("compacted{}", pi_store::icons::PART_SEP))
                .unwrap_or(&line);
            rows.push(Row::new(["compaction", line]));
        }
        if !tally.refits().is_empty() {
            rows.push(Row::new(["refit".into(), tally.refits().join("; ")]));
        }
        // Left out until something has been spent: a session nothing has been
        // asked of yet has no figure, and a row of dashes is not one.
        let spent = lane.spent();
        if spent != Totals::default() {
            rows.push(Row::new(["spent".into(), pi_store::text::spent(&spent)]));
        }

        let tier = format!("{:?}", resolved.ceiling).to_lowercase();
        rows.push(Row::new(["tier", &tier]));
        let mut workspace = crate::core::instructions::home()
            .and_then(|h| Some(format!("~/{}", root.strip_prefix(h).ok()?.display())))
            .unwrap_or_else(|| root.display().to_string());
        if let Some(tree) = lane.worktree() {
            workspace.push_str(&format!("{sep}worktree {tree}"));
        }
        rows.push(Row::new(["workspace".into(), workspace]));
        let extra = lane.workspace().write_roots();
        if tool::Tier::Write.under(resolved.ceiling)
            && lane.workspace().fenced()
            && !extra.is_empty()
        {
            let extra: Vec<_> = extra.iter().map(|p| short(p, root)).collect();
            rows.push(Row::new(["also writes".into(), extra.join(", ")]));
        }
        if let Some(system) = &resolved.system {
            rows.push(Row::new(["system prompt".into(), short(system, root)]));
        }
        if !resolved.prompts.is_empty() {
            let prompts: Vec<_> = resolved.prompts.iter().map(|p| short(p, root)).collect();
            rows.push(Row::new(["prompts".into(), prompts.join(", ")]));
        }
        if !resolved.instructions.is_empty() {
            rows.push(Row::new([
                "instructions".into(),
                resolved.instructions.join(", "),
            ]));
        }
        rows.push(Row::new(["pi", concat!("v", env!("CARGO_PKG_VERSION"))]));
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

        let session = Row::new([
            "session".into(),
            short(&self.store.path_of(root, lane.id()), root),
        ]);
        rows.push(match lane.unsaved() {
            Some(why) => session.noting(format!("not saved: {why}")),
            None => session,
        });
        rows.push(match (journal::path(), journal::failure()) {
            (Some(p), Some(why)) => {
                Row::new(["journal".into(), short(&p, root)]).noting(format!("stopped: {why}"))
            }
            (Some(p), None) => Row::new(["journal".into(), short(&p, root)]),
            (None, _) => Row::new(["journal", "off"]).noting("PI_LOG is off, or it would not open"),
        });
        if let Some(version) = crate::core::update::newer() {
            rows.push(Row::new(["update".into(), format!("{version} installed")]));
        } else if let Some(version) = crate::core::update::released() {
            rows.push(Row::new(["update".into(), format!("{version} available")]));
        }
        Listing::of(rows)
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
