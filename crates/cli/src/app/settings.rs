//! What `/settings` and `/reload` do to the App: the config in force, the
//! claims this session laid over the file, and the rows the panel shows.
//!
//! The value itself is `store/settings.rs`; this is what the App does to it.

use serde::Deserialize;

use super::App;
use super::meter::summary;
use crate::input::Step;
use crate::input::commands::Choice;
use crate::input::refused;
use crate::store::config::{self, Config};
use crate::store::icons;
use crate::store::settings::{self, mask_secret};

impl App {
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
        // Re-read the file tree; the claimed overrides stay.
        if let Err(e) = self.settings.reread(self.args.config.as_deref()) {
            return vec![format!("nothing reloaded — {}", refused("reload", e))];
        }
        let mut said = self.rebuild();
        // Name any claim that still shadows a line the file just changed.
        for path in self.settings.claimed().keys() {
            if let Some(old) = self.settings.file_value(path)
                && old != &self.settings.claimed()[path]
            {
                said.push(format!(
                    "{path}: the file changed it, but this session is still shadowing it — /settings, then r on the row takes the file back"
                ));
            }
        }
        said
    }
    // Take this config as the one in force: recompute everything it decides
    // and swap it in. Whole or not at all — nothing is touched until all of
    // it has been computed. `/reload` reads the file first; `/settings`
    // hands over a tree it has just edited.
    fn adopt(&mut self, config: Config) -> Result<Vec<String>, String> {
        let root = self.lane().ctx.workspace.root().to_path_buf();
        let failed = |e| Err(format!("nothing reloaded — {}", refused("reload", e)));
        let project = match config::load_project(&root) {
            Ok(p) => p,
            Err(e) => return failed(e),
        };
        let mut resolved = match crate::resolve(
            &self.args,
            &self.lane().ctx.workspace,
            &config,
            &project,
            self.settings.claimed(),
        ) {
            Ok(r) => r,
            Err(e) => return failed(e),
        };
        // Only here, with everything computed: a config or a skill set that
        // will not resolve leaves what is running exactly as it was.
        //
        // One `make_mut`: a run in flight holds the other reference, so this
        // is where the copy is taken, and taking it four times copies thrice
        // over.
        let home = self.home(root.clone(), self.lane().agent.spec.model.clone());
        let ag = std::sync::Arc::make_mut(&mut self.lane_mut().agent);
        ag.apply(agent::Setup {
            registry: std::mem::take(&mut resolved.registry),
            system: std::mem::take(&mut resolved.system),
            tier: resolved.tier,
            effort: resolved.effort,
            task_max_turns: resolved.max_turns,
            task_deadline: resolved.task_deadline,
        });
        crate::app::subagent::hang(ag, home, &resolved.standing);
        self.lane_mut().context = resolved.context;
        self.lane_mut().standing = resolved.standing;
        // A skill can appear between one turn and the next, so the table of
        // what a slash answers to is recomputed like everything else here —
        // onto the lane it belongs to, then into force.
        self.lane_mut().keys = std::sync::Arc::new(resolved.keys);
        self.lane_mut().commands = std::sync::Arc::new(resolved.commands);
        self.in_force();
        // The running model is deliberately not re-dialled: a reload re-reads
        // preferences, and which model this session is on was a decision, not a
        // preference. `/model` is how that one changes.
        self.config = std::sync::Arc::new(config);

        // A spec change forces a re-dial; compare with the same `dial` call
        // the running spec came from, so the command line's --base-url /
        // --context overrides keep applying exactly as they do at startup.
        let mut notes = Vec::new();
        match crate::dial(
            &self.args,
            &self.config,
            &self.lane().agent.spec.model,
            config::Origin::Command,
        ) {
            Ok(dialled) if dialled.spec != self.lane().agent.spec => {
                self.retarget(dialled.transport, dialled.spec);
                notes.extend(
                    dialled
                        .notes
                        .into_iter()
                        .filter(|n| !n.starts_with("assuming a")),
                );
                notes.extend(dialled.warning);
            }
            // Same spec, nothing to change; a failed dial keeps the old
            // transport but has to say so, or the config the model just
            // accepted disagrees with the endpoint it still talks to.
            Ok(_) => {}
            Err(e) => notes.push(format!(
                "`{}` not re-dialled — {}",
                self.lane_mut().agent.spec.model,
                e
            )),
        }
        tracing::info!(
            target: "pi::session",
            models = self.config.names().len(),
            rebound_keys = self.config.keys.len(),
            commands = self.commands.len(),
            effort = ?self.lane().agent.effort,
            system_bytes = self.lane().agent.system.len(),
            "reloaded"
        );
        Ok(notes)
    }
    // Recompute the config from the file tree plus the session's claimed
    // overrides, and adopt it.
    fn rebuild(&mut self) -> Vec<String> {
        self.rebuilt().unwrap_or_else(|why| vec![why])
    }
    // The file's rows with the session's claims on top — what the panel
    // shows and the read-only list prints. Path by path rather than one
    // overlaid tree, so a claim the file can no longer address (an ancestor
    // the file has turned into a non-table) still answers, with the file's
    // own value beside it for the mark.
    pub fn setting_rows(&self) -> Vec<settings::SettingRow> {
        self.settings.rows()
    }
    // The same, saying why when nothing could be adopted.
    fn rebuilt(&mut self) -> Result<Vec<String>, String> {
        let tree = self
            .settings
            .effective()
            .map_err(|e| refused("settings", e))?;
        let mut config = match config::Config::deserialize(tree) {
            Ok(c) => c,
            Err(e) => return Err(refused("settings", anyhow::anyhow!(e))),
        };
        config.apply_env_unclaimed(self.settings.claimed());
        self.adopt(config)
    }
    /// Take a value into the session: try the write on a scratch tree first,
    /// so a bad value touches nothing, then record it as a claim and rebuild.
    /// The panel's edit line answers through here, so a refusal comes back
    /// named, to be shown beside the edit that earned it.
    pub fn edit(&mut self, path: &str, raw: &str) -> Result<Vec<String>, String> {
        let (old, new) = self
            .settings
            .claim(path, raw)
            .map_err(|e| refused("settings", e))?;
        let mut said = self.rebuild();
        let old_shown = match &old {
            Some(v) => mask_secret(path, &settings::render(v)),
            None => "<unset>".to_string(),
        };
        said.push(format!(
            "{path}: {old_shown} → {} (session only)",
            mask_secret(path, &settings::render(&new))
        ));
        Ok(said)
    }
    /// The panel's r: the file's value takes the session back. No claim on
    /// the path means the file is already in force and there is nothing to
    /// say.
    pub fn revert(&mut self, path: &str) -> Vec<String> {
        if !self.settings.drop_claim(path) {
            return Vec::new();
        }
        let mut said = self.rebuild();
        said.push(format!("{path}: back to what the file says"));
        said
    }
    /// The panel's space: the session value at `path` replaces the file's
    /// line, the claim goes, and the config adopts what the file now says.
    /// The value was validated when the session took it, so only the disk
    /// can refuse.
    pub fn write_to_file(&mut self, path: &str) -> Result<Vec<String>, String> {
        let Some(value) = self.settings.claimed_value(path) else {
            return Ok(vec![format!("{path}: the session and the file agree")]);
        };
        let file = self
            .args
            .config
            .as_deref()
            .map(std::path::PathBuf::from)
            .or_else(config::global_path)
            .ok_or_else(|| "no settings file to write".to_string())?;
        config::write(&file, path, value.clone()).map_err(|e| format!("{e:#}"))?;
        self.settings.drop_claim(path);
        self.settings
            .reread(self.args.config.as_deref())
            .map_err(|e| format!("{e:#}"))?;
        let mut said = self.rebuild();
        said.push(format!(
            "{path} = {} — written to the file",
            mask_secret(path, &settings::render(&value))
        ));
        Ok(said)
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
    // `Task` holds a snapshot of the agent it was built from, so a retarget
    // that stopped at the lane would leave the child on the old provider —
    // with the old key — while the status line named the new model.
    pub(super) fn retarget(
        &mut self,
        transport: std::sync::Arc<dyn llm::Transport>,
        spec: llm::ModelSpec,
    ) {
        let home = self.home(
            self.lane().ctx.workspace.root().to_path_buf(),
            spec.model.clone(),
        );
        let standing = self.lane().standing.clone();
        let ag = std::sync::Arc::make_mut(&mut self.lane_mut().agent);
        ag.retarget(transport, spec);
        crate::app::subagent::hang(ag, home, &standing);
    }
    // `/settings`. The panel is the whole surface: bare opens it, and anything
    // after the word is refused rather than half-remembered as a verb.
    pub(super) fn settings(&mut self, rest: &str) -> Step {
        if rest.trim().is_empty() {
            self.open_panel()
        } else {
            Step::Flash("settings are edited in the panel — bare /settings opens it".into())
        }
    }
    // The bare `/settings`: the TUI's panel, or a read-only list when this
    // is not a terminal.
    fn open_panel(&mut self) -> Step {
        // The TUI intercepts bare `/settings` before it reaches here; the
        // line surface can only list.
        let mut out = Vec::new();
        for row in self.setting_rows() {
            let mut line = format!("{} = {}", row.path, mask_secret(&row.path, &row.value));
            if row.changed {
                line.push_str(&format!(" {}", icons::CHANGED_MARK));
                if let Some(file) = self.settings.file_value(&row.path) {
                    let file = mask_secret(&row.path, &settings::render(file));
                    line.push_str(&format!(" file: {file}"));
                }
            }
            out.push(line);
        }
        if out.is_empty() {
            out.push("nothing in ~/.pi/settings.toml yet".into());
        }
        Step::Handled(out)
    }
}
