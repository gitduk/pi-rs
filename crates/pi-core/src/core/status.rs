//! Runtime configuration, spending, diagnostics and session storage.

use crate::core::instructions::short;
use agent::Totals;

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
            format!("{}{sep}effort {effort}", spec.model),
        ]));
        // An explicit URL names its flag; otherwise use the active model's
        // provider identity alongside the host its requests actually reach.
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
            _ => {
                let provider = spec.model.split_once('/').map(|(name, _)| name);
                let endpoint = provider.map_or_else(
                    || spec.base_url.clone(),
                    |name| format!("{} ([provider.{name}])", spec.base_url),
                );
                rows.push(Row::new(["endpoint".into(), endpoint]));
            }
        }
        // Filed under the reply it cut; gathered here for the whole run.
        let tally = lane.tally();
        if !tally.gaps().is_empty() {
            rows.push(Row::new([
                "incomplete reply".into(),
                tally.gaps().join("; "),
            ]));
        }

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
        rows.push(Row::new(["pi", concat!("v", env!("CARGO_PKG_VERSION"))]));
        rows.push(match pi_tools::rtk::state() {
            pi_tools::rtk::State::On(version) => Row::new(["rtk".into(), version]),
            pi_tools::rtk::State::Off => Row::new(["rtk", "off"]).noting("RTK_DISABLED=1"),
            pi_tools::rtk::State::Missing => {
                Row::new(["rtk", "not found"]).noting("commands run as written")
            }
            pi_tools::rtk::State::Unknown => Row::new(["rtk", "not asked yet"]),
        });
        let servers = self.mcp().summary();
        if !servers.is_empty() {
            rows.push(Row::new(["mcp".into(), servers.join(", ")]));
        }

        let session = Row::new([
            "session".into(),
            format!(
                "{} · {}",
                lane.id(),
                short(&self.store.path_of(root, lane.id()).database, root)
            ),
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
