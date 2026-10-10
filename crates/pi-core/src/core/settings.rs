//! What a reload, `/settings` and `/model` do to the Core.
//!
//! The value itself is `pi_store::settings`; this is what the Core does to it.

use super::meter::summary;
use super::status::{carries_reasoning, demotion};
use super::{Core, resolve};
use crate::input::commands::Choice;
use crate::input::{Step, refused};
use pi_store::args::EffortArg;
use pi_store::config::{self, Config};
use pi_store::icons;

impl Core {
    /// Re-read the config and everything it decides, whole or not at all:
    /// nothing is touched until it is all computed.
    ///
    /// Session-owned state (transcript, name, history, model) is untouched;
    /// only what the config decides is replaced.
    pub fn reload(&mut self) -> Vec<String> {
        self.try_reload().unwrap_or_else(|why| vec![why])
    }

    /// The same, with a refusal as `Err`: why nothing was reloaded.
    pub fn try_reload(&mut self) -> Result<Vec<String>, String> {
        let root = self.lane().root().to_path_buf();
        if let Err(e) = self.settings.reread(self.pinned.config.as_deref(), &root) {
            return Err(format!("nothing reloaded — {}", refused("reload", e)));
        }
        let before = self.lane().resolved().endpoint.clone();
        let mut said = self.rebuilt()?;
        // The banner is drawn once, so a moved endpoint is said here instead.
        // An edit needs no such line: it names the value it wrote.
        let after = &self.lane().resolved().endpoint;
        if *after != before {
            said.extend(after.clone());
        }
        Ok(said)
    }
    // Take this config as the one in force, whole or not at all — nothing
    // is touched until it is all computed.
    fn adopt(&mut self, config: Config) -> Result<Vec<String>, String> {
        let root = self.lane().root().to_path_buf();
        let failed = |e| Err(format!("nothing reloaded — {}", refused("reload", e)));
        // `write_roots` widen the workspace, so it is built again from them.
        let workspace = match tool::Workspace::new(&root)
            .and_then(|ws| ws.with_write_roots(&config.writable()))
        {
            Ok(ws) => ws,
            Err(e) => return failed(e.into()),
        };
        let resolved = match crate::core::resolve::resolve(
            &self.pinned,
            &workspace,
            &config,
            &self.settings,
        ) {
            Ok(r) => r,
            Err(e) => return failed(e),
        };
        // The model stays; its entry is re-read via the same `dial` path.
        // A failed dial keeps the old transport rather than refusing the file.
        let running = self.lane().agent().spec().clone();
        let mut notes: Vec<String> = Vec::new();
        let retarget = match crate::core::dial::dial(
            &self.pinned,
            &config,
            &running.model,
            config::ModelOrigin::Command,
        ) {
            Ok(dialled) if dialled.spec != running => {
                notes.extend(dialled.warning);
                Some((dialled.transport, dialled.spec))
            }
            Ok(_) => None,
            Err(e) => {
                notes.push(format!("`{}` not re-dialled — {e}", running.model));
                None
            }
        };
        let model = retarget
            .as_ref()
            .map_or(running.model, |(_, s)| s.model.clone());
        // The compactor holds the summarizer's connection, so it's rebuilt
        // here or `summarize_model`/`idle_timeout` never follow a reload.
        let retry = crate::core::retry(&config);
        let writer = crate::core::dial::summary_writer(&self.pinned, &config, &model)
            .map_err(|e| format!("nothing reloaded — {}", refused("summarize_model", e)))?;

        // Only here, with everything computed, is anything touched — and in
        // one `rearm`, so the copy a run in flight forces is taken once.
        let archive = self.archive(root.clone(), model);
        let idle = retry.idle;
        let jobs = self.jobs.clone();
        self.lane_mut().ctx_mut().workspace = workspace;
        self.lane_mut()
            .rearm(std::sync::Arc::new(resolved), archive, retry, jobs, |ag| {
                ag.compactor = std::sync::Arc::new(agent::Summarizing::new(writer, idle));
                if let Some((transport, spec)) = retarget {
                    ag.retarget(transport, spec);
                }
            });
        self.in_force();
        crate::core::mcp::sync(&config, &root);
        self.config = std::sync::Arc::new(config);
        tracing::info!(
            target: "pi::session",
            models = self.config.names().len(),
            rebound_keys = self.config.keys.len(),
            commands = self.commands.len(),
            effort = ?self.lane().agent().brief.effort,
            system_bytes = self.lane().agent().brief.system.len(),
            "reloaded"
        );
        Ok(notes)
    }
    // Recompute the config from the file trees and adopt it, saying why when
    // nothing could be adopted.
    fn rebuilt(&mut self) -> Result<Vec<String>, String> {
        let config = self
            .settings
            .config()
            .map_err(|e| format!("nothing reloaded — {}", refused("settings", e)))?;
        self.adopt(config)
    }
    /// `/edit`: bare, the files it offers; with a name, that file to open.
    pub(super) fn edit(&self, name: &str) -> Step {
        let files = resolve::editable(&self.pinned, self.lane().root());
        if name.is_empty() {
            let width = files
                .iter()
                .map(|f| f.name.chars().count())
                .max()
                .unwrap_or(0);
            let mut lines: Vec<String> = files
                .iter()
                .map(|f| format!("{:width$}  {}", f.name, f.note))
                .collect();
            lines.push("/edit <file> opens one in $EDITOR".into());
            return Step::Handled(pi_store::listing::Listing::say(lines));
        }
        match files.into_iter().find(|f| f.name == name) {
            Some(file) => Step::Edit(file.path),
            None => Step::Flash(format!(
                "`{name}` is not a file /edit offers — bare /edit lists them"
            )),
        }
    }

    /// After `/settings` or `/edit` closed the editor: reload, and name what
    /// the files now say that a flag or the environment still outranks.
    /// `Err` is why nothing was reloaded.
    pub fn config_edited(&mut self) -> Result<Vec<String>, String> {
        let mut said = self.try_reload()?;
        said.extend(self.shadowed());
        Ok(said)
    }
    // Each key the files set that this run takes from somewhere higher.
    fn shadowed(&self) -> Vec<String> {
        let env = config::endpoint_env().map(|var| format!("${var}"));
        let flag = |on: bool, name: &str| on.then(|| name.to_string());
        [
            (
                "base_url",
                flag(self.pinned.base_url.is_some(), "--base-url").or(env.clone()),
            ),
            ("format", env),
            ("effort", flag(self.pinned.effort.is_some(), "--effort")),
            ("tier", flag(self.pinned.tier.is_some(), "--tier")),
        ]
        .into_iter()
        .filter(|(path, _)| self.settings.sets(path))
        .filter_map(|(path, by)| {
            Some(format!(
                "{path}: {} outranks the files, so this run keeps it",
                by?
            ))
        })
        .collect()
    }
    /// The models `/model` can reach, with what tells them apart.
    pub fn choices(&self) -> Vec<Choice> {
        self.config
            .models
            .iter()
            .map(|(name, entry)| {
                let format = entry.format.or(self.config.format);
                Choice {
                    name: name.clone(),
                    note: summary(
                        format.map(|f| f.name()).unwrap_or_default(),
                        entry.context_window,
                        &entry.pricing,
                    ),
                }
            })
            .collect()
    }
    // Rebuilds the subagent too: it snapshots the agent it was built from,
    // so stopping at the lane would leave it on the old provider/key.
    pub(super) fn retarget(
        &mut self,
        transport: std::sync::Arc<dyn llm::Transport>,
        spec: llm::ModelSpec,
    ) {
        let archive = self.archive(self.lane().root().to_path_buf(), spec.model.clone());
        let resolved = self.lane().resolved().clone();
        let retry = crate::core::retry(&self.config);
        // The child is built from this agent, so the tool is hung again for it
        // to run on the model this session just moved to.
        let jobs = self.jobs.clone();
        self.lane_mut().rearm(resolved, archive, retry, jobs, |ag| {
            ag.retarget(transport, spec)
        });
    }
}

impl Core {
    /// Move this session to another model; the transcript comes with it.
    ///
    /// A transport demotes reasoning blocks it did not write, so history
    /// stays sendable instead of a 400; switching back restores them.
    ///
    /// Past spend stays priced at the spec each turn ran under; a switch
    /// does not reprice turns already spent.
    pub fn switch(&mut self, name: &str) -> Vec<String> {
        let dialled = match crate::core::dial::dial(
            &self.pinned,
            &self.config,
            name,
            config::ModelOrigin::Command,
        ) {
            Ok(d) => d,
            Err(e) => {
                let held = self.lane_mut().agent().spec().model.clone();
                return vec![format!("still on {held} — {}", refused("switch", e))];
            }
        };
        // Compared after resolving: `find` also accepts a `wire_id`, so the
        // typed name may differ from the id it resolves to.
        if dialled.spec.model == self.lane_mut().agent().spec().model {
            return vec![format!(
                "already on {}",
                self.lane_mut().agent().spec().model
            )];
        }
        let mut said: Vec<String> = dialled.warning.into_iter().chain(dialled.assumed).collect();
        let spec = &dialled.spec;
        said.push(format!(
            "now on {}{}{}",
            spec.model,
            icons::PART_SEP,
            summary(spec.format.name(), spec.context_window, &spec.pricing)
        ));
        // An absent transcript is one a run has, and it is writing this
        // model's reasoning into it as we speak — so say it either way.
        if self.lane().session().is_none_or(carries_reasoning) {
            said.push(demotion(spec.replay_thinking).into());
        }
        tracing::info!(
            target: "pi::session",
            from = %self.lane_mut().agent().spec().model,
            to = %spec.model,
            format = spec.format.name(),
            context_window = spec.context_window,
            "model switched"
        );
        self.retarget(dialled.transport, dialled.spec);
        said
    }
    /// `/effort`: bare, the effort in force; with a level, that level from
    /// the next request on, as `--effort` would have set it.
    pub(super) fn effort(&mut self, arg: &str) -> Vec<String> {
        use clap::ValueEnum;
        let now = |core: &Self| core.lane().agent().brief.effort.name();
        let levels = EffortArg::names().join("|");
        let arg = arg.trim();
        if arg.is_empty() {
            return vec![format!("effort {} — /effort {levels}", now(self))];
        }
        let Ok(level) = EffortArg::from_str(arg, true) else {
            return vec![format!("unknown effort `{arg}` — /effort {levels}")];
        };
        let was = self.pinned.effort.replace(level);
        let mut said = match self.rebuilt() {
            Ok(said) => said,
            Err(why) => {
                self.pinned.effort = was;
                return vec![why];
            }
        };
        said.push(format!("effort {}", now(self)));
        if self.lane().agent().spec().thinking.is_none() {
            let model = &self.lane().agent().spec().model;
            said.push(format!(
                "{model} takes no thinking setting, so this changes nothing on it"
            ));
        }
        said
    }
    // What `/model` on its own shows.
    pub(super) fn listing(&self) -> Vec<String> {
        let here = &self.lane().agent().spec().model;
        let choices = self.choices();
        if choices.is_empty() {
            return vec![
                format!("on {here}, and ~/.pi/settings.toml now defines no model to switch to"),
                "see examples/pi.toml for what a [models.<name>] entry looks like".into(),
            ];
        }
        let width = choices.iter().map(|c| c.name.len()).max().unwrap_or(0);
        choices
            .iter()
            .map(|c| {
                let mark = if &c.name == here {
                    icons::CURRENT_ITEM
                } else {
                    " "
                };
                format!("{mark} {:width$}  {}", c.name, c.note)
            })
            .collect()
    }
}
