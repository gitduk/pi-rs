//! What a reload, `/settings` and `/model` do to the Core.
//!
//! The value itself is `pi_store::settings`; this is what the Core does to it.

use super::meter::summary;
use super::status::carries_reasoning;
use super::{Core, resolve};
use crate::input::commands::Choice;
use crate::input::{Step, refused};
use pi_store::args::EffortArg;
use pi_store::config::{self, Config};
use pi_store::icons;
use pi_store::settings::Settings;

impl Core {
    /// Re-read the config and everything it decides, whole or not at all:
    /// nothing is touched until it is all computed.
    ///
    /// Session-owned state (transcript, name, history, model) is untouched;
    /// only what the config decides is replaced.
    pub fn reload(&mut self) -> Vec<String> {
        self.try_reload().unwrap_or_else(|why| vec![why])
    }

    /// Before a run: read the prompt's files again if the session is new or
    /// compacted since. `Some` names the files that read differently.
    pub fn refresh_prompt_files(&mut self) -> Option<Vec<String>> {
        if !self.lane_mut().prompt_files_due() {
            return None;
        }
        let lane = self.lane();
        let root = lane.root().to_path_buf();
        let was = lane.resolved().prompt_files.clone();
        let resolved = match resolve::resolve(
            &self.pinned,
            lane.workspace(),
            &self.config,
            &self.settings,
            None,
            self.surface,
        ) {
            Ok(r) => r,
            Err(e) => {
                let why = format!("{e:#}");
                tracing::warn!(target: "pi::session", error = %why, "prompt files not re-read");
                return Some(vec![format!("prompt files not re-read — {why}")]);
            }
        };
        if *resolved.prompt_files == *was {
            return None;
        }
        let moved = was.moved(&resolved.prompt_files, &root);
        tracing::info!(target: "pi::session", moved = %moved.join(", "), "prompt files re-read");
        // Said as it changes, not on every read.
        let left_out = resolved.prompt_files.left_out(&root);
        let left_out = left_out.filter(|now| was.left_out(&root).as_ref() != Some(now));
        let archive = self.archive(root, lane.agent().spec().model.clone());
        let retry = crate::core::retry(&self.config);
        let jobs = self.jobs.clone();
        self.lane_mut()
            .rearm(std::sync::Arc::new(resolved), archive, retry, jobs, |_| {});
        let said: Vec<String> = (!moved.is_empty())
            .then(|| format!("re-read — {}", moved.join(", ")))
            .into_iter()
            .chain(left_out)
            .collect();
        (!said.is_empty()).then_some(said)
    }

    /// The same, with a refusal as `Err`: why nothing was reloaded.
    pub fn try_reload(&mut self) -> Result<Vec<String>, String> {
        let root = self.lane().root().to_path_buf();
        // Held apart until adopted: a reload refused leaves the files in force.
        let settings = Settings::load(self.pinned.config.as_deref(), &root)
            .map_err(|e| format!("nothing reloaded — {}", refused("reload", e)))?;
        let before = self.lane().resolved().endpoint.clone();
        let mut said = self.rebuilt(Some(settings))?;
        // What the run stands on is shown once, so a moved endpoint is said here.
        // An edit needs no such line: it names the value it wrote.
        let after = &self.lane().resolved().endpoint;
        if *after != before {
            said.extend(after.clone());
        }
        Ok(said)
    }
    // Take this config as the one in force, whole or not at all — nothing
    // is touched until it is all computed. `settings`, when given, replace the
    // ones in force with it; `prompt_files` as `resolve` takes them.
    pub(super) fn adopt(
        &mut self,
        config: Config,
        workspace: tool::Workspace,
        settings: Option<Settings>,
        prompt_files: Option<std::sync::Arc<resolve::PromptFiles>>,
    ) -> Result<Vec<String>, String> {
        let root = workspace.root().to_path_buf();
        let failed = |e| Err(format!("nothing reloaded — {}", refused("reload", e)));
        let resolved = match crate::core::resolve::resolve(
            &self.pinned,
            &workspace,
            &config,
            settings.as_ref().unwrap_or(&self.settings),
            prompt_files,
            self.surface,
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
        if let Some(settings) = settings {
            self.settings = settings;
        }
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
    // Recompute the config from the file trees — `settings`', else those in
    // force — and adopt it, saying why when nothing could be adopted.
    fn rebuilt(&mut self, settings: Option<Settings>) -> Result<Vec<String>, String> {
        let config = (settings.as_ref().unwrap_or(&self.settings))
            .config()
            .map_err(|e| format!("nothing reloaded — {}", refused("settings", e)))?;
        // The tier fences the workspace, so it is built again from the config.
        let workspace = tool::Workspace::new(self.lane().root())
            .and_then(|ws| resolve::fence(ws, &config, &self.pinned))
            .map_err(|e| format!("nothing reloaded — {}", refused("reload", e.into())))?;
        let prompt_files = self.lane().resolved().prompt_files.clone();
        self.adopt(config, workspace, settings, Some(prompt_files))
    }
    /// `/edit`: bare, the files it offers; with a name, that file to open.
    pub(super) fn edit(&self, name: &str) -> Step {
        let files = resolve::editable(&self.pinned, self.lane().root());
        if name.is_empty() {
            let width = pi_store::text::widest(files.iter().map(|f| f.name.as_str()));
            let mut lines: Vec<String> = files
                .iter()
                .map(|f| format!("{}  {}", pi_store::text::pad(&f.name, width), f.note))
                .collect();
            lines.push("/edit <file> opens one in $EDITOR".into());
            return Step::Handled(pi_store::listing::Listing::say(lines));
        }
        match files.into_iter().find(|f| f.name == name) {
            Some(file) => match resolve::seed(&file.path) {
                Ok(()) => Step::Edit(file.path),
                Err(e) => Step::Notice(format!("cannot start {name} from the built-in: {e}")),
            },
            None => Step::Notice(format!("`{name}` is not a file /edit offers")),
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
    // The model `name` for the lane in front, unless it is on it already;
    // the refusal is worded for the user.
    pub(super) fn dial_for(
        &self,
        name: &str,
        origin: config::ModelOrigin,
    ) -> Result<Option<crate::core::dial::Dialled>, String> {
        let held = self.lane().model();
        let dialled = crate::core::dial::dial(&self.pinned, &self.config, name, origin)
            .map_err(|e| format!("still on {held} — {}", refused("switch", e)))?;
        // Compared after resolving: `find` also accepts a `wire_id`, so the
        // typed name may differ from the id it resolves to.
        Ok((dialled.spec.model != held).then_some(dialled))
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
        let dialled = match self.dial_for(name, config::ModelOrigin::Command) {
            Ok(Some(d)) => d,
            Ok(None) => return vec![format!("already on {}", self.lane().model())],
            Err(why) => return vec![why],
        };
        let mut said: Vec<String> = dialled.warning.into_iter().chain(dialled.assumed).collect();
        let spec = &dialled.spec;
        // The surface shows the model's name already; what it costs it does not.
        said.push(summary(
            spec.format.name(),
            spec.context_window,
            &spec.pricing,
        ));
        // An absent transcript is one a run has, and it is writing this
        // model's reasoning into it as we speak — so say it either way.
        if self.lane().session().is_none_or(carries_reasoning) {
            said.push("reasoning from the earlier turns is dropped rather than replayed".into());
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
        let mut said = match self.rebuilt(None) {
            Ok(said) => said,
            Err(why) => {
                self.pinned.effort = was;
                return vec![why];
            }
        };
        let spec = self.lane().agent().spec();
        if !spec.accepts_effort {
            said.push(format!(
                "{} is marked effort = \"unsupported\", so this changes nothing on it",
                spec.model
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
        let width = pi_store::text::widest(choices.iter().map(|c| c.name.as_str()));
        choices
            .iter()
            .map(|c| {
                let mark = if &c.name == here {
                    icons::CURRENT_ITEM
                } else {
                    " "
                };
                format!("{mark} {}  {}", pi_store::text::pad(&c.name, width), c.note)
            })
            .collect()
    }
}
