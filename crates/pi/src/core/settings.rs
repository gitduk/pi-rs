//! What `/reload`, `/settings` and `/model` do to the Core, and the rows the
//! settings panel shows.
//!
//! The value itself is `store/settings.rs`; this is what the Core does to it.

use super::Core;
use super::meter::summary;
use super::status::{carries_reasoning, demotion};
use crate::input::commands::Choice;
use crate::input::refused;
use crate::store::config::{self, Config};
use crate::store::icons;
use crate::store::settings::{self, mask_secret};

impl Core {
    /// Re-read the config and everything it decides.
    ///
    /// Whole or not at all: on any failure nothing changes, which is why the
    /// new state is computed in full before a field is touched. Pi separates
    /// global from project so a broken one of each does not take the other
    /// down; here the whole reload is refused instead, and what was running
    /// keeps running — the case that separation protects against cannot arise
    /// when nothing is applied.
    ///
    /// What the session owns is untouched by construction: the transcript, the
    /// name, the history, the model. Only what the config decides is
    /// replaced.
    pub fn reload(&mut self) -> Vec<String> {
        let root = self.lane().root().to_path_buf();
        if let Err(e) = self.settings.reread(self.pinned.config.as_deref(), &root) {
            return vec![format!("nothing reloaded — {}", refused("reload", e))];
        }
        let before = self.lane().resolved().endpoint.clone();
        let mut said = self.rebuild();
        // The banner is drawn once, so a moved endpoint is said here instead.
        // An edit needs no such line: it names the value it wrote.
        let after = &self.lane().resolved().endpoint;
        if *after != before {
            said.extend(after.clone());
        }
        said
    }
    // Take this config as the one in force: recompute everything it decides
    // and swap it in. Whole or not at all — nothing is touched until all of
    // it has been computed. `/reload` reads the file first; `/settings`
    // hands over a tree it has just edited.
    fn adopt(&mut self, config: Config) -> Result<Vec<String>, String> {
        let root = self.lane().root().to_path_buf();
        let failed = |e| Err(format!("nothing reloaded — {}", refused("reload", e)));
        let resolved = match crate::core::resolve::resolve(
            &self.pinned,
            self.lane().workspace(),
            &config,
            &self.settings,
        ) {
            Ok(r) => r,
            Err(e) => return failed(e),
        };
        // The model stays — which one runs was a decision, not a preference —
        // but its entry is re-read, through the same `dial` startup used.
        // A failed dial keeps the old transport and says so: the model's
        // entry breaking is no reason to refuse the rest of the file.
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
        // The one thing here that is an object rather than a value: the
        // compactor holds the summarizer's own connection, so it is rebuilt
        // here or `summarize_model` and `idle_timeout` never follow a reload.
        let retry = config.retry();
        let writer = crate::core::dial::summary_writer(&self.pinned, &config, &model)
            .map_err(|e| format!("nothing reloaded — {}", refused("summarize_model", e)))?;

        // Only here, with everything computed, is anything touched — and in
        // one `rearm`, so the copy a run in flight forces is taken once.
        let archive = self.archive(root.clone(), model);
        let idle = retry.idle;
        self.lane_mut()
            .rearm(std::sync::Arc::new(resolved), archive, retry, |ag| {
                ag.compactor = std::sync::Arc::new(agent::Summarizing::new(writer, idle));
                if let Some((transport, spec)) = retarget {
                    ag.retarget(transport, spec);
                }
            });
        self.in_force();
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
    // Recompute the config from the file trees, and adopt it.
    fn rebuild(&mut self) -> Vec<String> {
        self.rebuilt().unwrap_or_else(|why| vec![why])
    }
    // The same, saying why when nothing could be adopted.
    fn rebuilt(&mut self) -> Result<Vec<String>, String> {
        let config = self.settings.config().map_err(|e| refused("settings", e))?;
        self.adopt(config)
    }
    // Every leaf the files hold — what the panel shows.
    pub fn setting_rows(&self) -> Vec<settings::SettingRow> {
        self.settings.rows()
    }
    /// Write a value into this project's `.pi.toml` and reload: tried on a
    /// copy of the files first, and the file put back if the reload still
    /// refuses it, so a bad value does not stay on disk. The panel's edit line
    /// answers through here, so a refusal comes back named, to be shown beside
    /// the edit that earned it.
    pub fn edit(&mut self, path: &str, raw: &str) -> Result<Vec<String>, String> {
        let (old, new) = self
            .settings
            .check(path, raw)
            .map_err(|e| refused("settings", e))?;
        let root = self.lane().root().to_path_buf();
        let file = config::project_target(&root)
            .ok_or_else(|| "no project here: a .pi.toml in $HOME is never read".to_string())?;
        let before = std::fs::read(&file).ok();
        config::write(&file, path, new.clone()).map_err(|e| format!("{e:#}"))?;
        let at = self.pinned.config.clone();
        let adopted = self
            .settings
            .reread(at.as_deref(), &root)
            .map_err(|e| refused("settings", e))
            .and_then(|()| self.rebuilt());
        let mut said = match adopted {
            Ok(said) => said,
            Err(why) => {
                let _ = match before {
                    Some(bytes) => tool::state::write_private(&file, &bytes),
                    None => std::fs::remove_file(&file),
                };
                let _ = self.settings.reread(at.as_deref(), &root);
                return Err(why);
            }
        };
        let old_shown = match &old {
            Some(v) => mask_secret(path, &settings::render(v)),
            None => "<unset>".to_string(),
        };
        said.push(format!(
            "{path}: {old_shown} → {} — written to {}",
            mask_secret(path, &settings::render(&new)),
            agent::context::short(&file, &root)
        ));
        said.extend(self.shadowed(path));
        Ok(said)
    }
    // A value just written that a flag or the environment still outranks.
    fn shadowed(&self, path: &str) -> Option<String> {
        let by = match path {
            "base_url" if self.pinned.base_url.is_some() => "--base-url".to_string(),
            "base_url" | "format" => format!("${}", config::endpoint_env()?),
            "effort" if self.pinned.effort.is_some() => "--effort".to_string(),
            "tier" if self.pinned.tier.is_some() => "--tier".to_string(),
            "system" if self.pinned.system.is_some() => "--system".to_string(),
            _ => return None,
        };
        Some(format!(
            "{path}: {by} still outranks the file, so this run keeps it"
        ))
    }
    /// The models `/model` can reach, with what tells them apart.
    pub fn choices(&self) -> Vec<Choice> {
        let format = self.config.format.map(|f| f.name()).unwrap_or_default();
        self.config
            .models
            .iter()
            .map(|(name, entry)| Choice {
                name: name.clone(),
                note: summary(format, entry.context_window, &entry.pricing),
            })
            .collect()
    }
    // Point this lane at a new endpoint, and rebuild the subagent behind it.
    //
    // `Subagent` holds a snapshot of the agent it was built from, so a retarget
    // that stopped at the lane would leave the child on the old provider —
    // with the old key — while the status line named the new model.
    pub(super) fn retarget(
        &mut self,
        transport: std::sync::Arc<dyn llm::Transport>,
        spec: llm::ModelSpec,
    ) {
        let archive = self.archive(self.lane().root().to_path_buf(), spec.model.clone());
        let resolved = self.lane().resolved().clone();
        let retry = self.config.retry();
        // The child is built from this agent, so the tool is hung again for it
        // to run on the model this session just moved to.
        self.lane_mut()
            .rearm(resolved, archive, retry, |ag| ag.retarget(transport, spec));
    }
}

impl Core {
    /// Move this session to another model.
    ///
    /// The transcript comes with it. Reasoning blocks carry the model that
    /// produced them and every transport demotes one it did not write —
    /// signature dropped, replayed as text or as `<think>` per the new model's
    /// `thinking_replay` — so the history stays sendable instead of becoming a
    /// 400 on the next turn. Nothing is rewritten on the way: switch back and
    /// the original blocks are native again.
    ///
    /// What has been spent stays spent. Each turn was priced by the spec in
    /// force when it ran, and the total is the sum of those, so a switch to a
    /// dearer model does not reprice the cheap turns behind it.
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
        // Compared after resolving, not before: `find` accepts a model's
        // `wire_id` as well as its table name, so the name typed and the id it
        // lands on need not be the same string. Comparing the typed one would
        // re-dial the model already running and then announce a reasoning
        // demotion that never happened.
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
