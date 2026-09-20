use std::borrow::Cow;
use std::collections::BTreeMap;

use agent::Totals;
use agent::session::Session;
use tools::skills::Skill;
use tools::{Tool, ToolError};

use serde::Deserialize;

use crate::config::Config;
use crate::icons;
use crate::journal;
use crate::lane::Lane;
use crate::session::{ResumeChoice, Store, Stored};

/// Where a command came from, and so what running it means.
#[derive(Clone)]
pub enum Source {
    // A word `parse` knows and `command` answers itself.
    Builtin,
    // A `SKILL.md` to read and hand to the model as if the user had typed it.
    Skill(Skill),
}

/// One command: the word, what it takes, what it does, and what it is.
#[derive(Clone)]
pub struct Command {
    /// With the leading slash.
    pub word: Cow<'static, str>,
    /// Shown in completion, in the shape prompt templates use elsewhere:
    /// angle brackets required, square brackets optional.
    pub args: &'static str,
    /// One line of it. A skill's description is written for the model and is
    /// routinely longer than a line, so it arrives here already cut down.
    pub help: Cow<'static, str>,
    pub source: Source,
}

impl Command {
    const fn builtin(word: &'static str, args: &'static str, help: &'static str) -> Self {
        Self {
            word: Cow::Borrowed(word),
            args,
            help: Cow::Borrowed(help),
            source: Source::Builtin,
        }
    }
}

// Every built-in command, once. `parse` still maps words to typed variants,
// but the help text and the completion list are generated from here — a new
// command that reached only one of the three was the bug this table prevents.
//
// Nothing reads this directly except `commands`, which appends the skills to
// it. What a run answers to is settled when the workspace is known, not when
// the binary is built.
const BUILTIN: &[Command] = &[
    Command::builtin(
        "/new",
        "",
        "start a fresh session, keeping this one on disk",
    ),
    Command::builtin(
        "/resume",
        "[id]",
        "list this workspace's sessions, or switch to one",
    ),
    Command::builtin(
        "/worktree",
        "[name]",
        "list this repository's worktrees, or work in one — rm removes one",
    ),
    Command::builtin(
        "/name",
        "[text]",
        "call this session something you will recognise",
    ),
    Command::builtin(
        "/model",
        "[name]",
        "list the models in ~/.pi/settings.toml, or move this session to one",
    ),
    Command::builtin(
        "/compact",
        "[focus]",
        "summarize everything but what you are working on now",
    ),
    Command::builtin(
        "/loop",
        "[text]",
        "repeat a line while it keeps changing the tree; bare, stop one",
    ),
    Command::builtin(
        "/reload",
        "",
        "re-read ~/.pi/settings.toml, the instructions and the skills",
    ),
    Command::builtin(
        "/status",
        "",
        "what this session stands on, has spent, and where it writes",
    ),
    Command::builtin(
        "/keys",
        "",
        "what every key does, and the id to rebind it under",
    ),
    Command::builtin("/help", "", "this list"),
    Command::builtin("/settings", "", "open the settings panel"),
    Command::builtin(
        "/wechat",
        "[on|off]",
        "bridge this session to WeChat (scan a QR on first connect)",
    ),
    Command::builtin("/exit", "", "leave (Ctrl-D does the same)"),
];

// How wide a one-line description may be before it is cut.
const GIST: usize = 60;

// A description written for the model, cut down to a line for a list.
//
// Two cuts, because they answer different questions: the first sentence is
// where the description stops being a summary, and the column is where the
// terminal stops having room.
fn gist(description: &str) -> String {
    let first = description
        .split_once(". ")
        .map_or(description, |(head, _)| head);
    crate::render::clip(first.trim().trim_end_matches('.'), GIST)
}

/// What a slash answers to: the built-ins, then one command per skill.
///
/// No prefix. A skill is `/commit`, not `/skill:commit`, because the name is
/// what it is known by and a namespace only earns its keep when something else
/// is competing for the word. What does compete is a built-in, and the built-in
/// wins: a repository contributes skills, and one that could take `/new` away
/// from the session it would otherwise start is a checkout redefining the
/// terminal. The skill itself is untouched — the model can still load it by
/// name — and the note says which of the two happened, because a command that
/// silently is not there is one the user goes looking for in the wrong place.
pub fn commands(skills: &[Skill], notes: &mut Vec<String>) -> Vec<Command> {
    let mut out = BUILTIN.to_vec();
    for skill in skills {
        let word = format!("/{}", skill.name);
        if out.iter().any(|c| c.word.as_ref() == word) {
            notes.push(format!(
                "skill `{}` has no {word} — that word is a built-in command; \
                 the model can still load the skill by name",
                skill.name
            ));
            continue;
        }
        out.push(Command {
            word: Cow::Owned(word),
            args: "[text]",
            help: Cow::Owned(gist(&skill.description)),
            source: Source::Skill(skill.clone()),
        });
    }
    out
}

fn help(commands: &[Command]) -> Vec<String> {
    let width = commands
        .iter()
        .map(|c| c.word.len() + c.args.len() + 1)
        .max()
        .unwrap_or(0);
    let row = |c: &Command| {
        let head = format!("{} {}", c.word, c.args);
        format!("{head:width$}  {}", c.help)
    };
    // The break is where the built-ins end, not where the skills begin: a third
    // source would otherwise land silently in the half that looks built in.
    // `commands` keeps the built-ins first and contiguous, so one position
    // settles it.
    let Some(split) = commands
        .iter()
        .position(|c| !matches!(c.source, Source::Builtin))
    else {
        return commands.iter().map(row).collect();
    };
    let mut out: Vec<String> = commands[..split].iter().map(row).collect();
    // Without a prefix there is nothing in the word itself to say which half it
    // came from, so the list says it once.
    out.push(String::new());
    out.push("skills — the instructions load when you run one:".into());
    out.extend(commands[split..].iter().map(row));
    out
}

/// Something the prompt can complete to — a model, a worktree — and what tells
/// it apart from the others.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub name: String,
    pub note: String,
}

/// One thing the line could still become.
///
/// Owned rather than borrowed from the table, because half the candidates come
/// from the config and none of those are `'static`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Shown in the list.
    pub show: String,
    /// What the whole line becomes when this is accepted.
    pub line: String,
    pub help: String,
    /// Something is still expected after it, so accepting leaves a trailing
    /// space and the caret past it.
    pub more: bool,
}

// A worktree name completed against `prefix`; the whole line an accept makes
// is `head` plus the name, which is what tells entering from removing.
fn worktree_candidates(trees: &[Choice], head: &str, prefix: &str) -> Vec<Candidate> {
    trees
        .iter()
        .filter(|w| w.name.starts_with(prefix) && w.name != prefix)
        .map(|w| Candidate {
            show: w.name.clone(),
            line: format!("{head}{}", w.name),
            help: w.note.clone(),
            more: false,
        })
        .collect()
}

/// What the line could still become: a command while its word is being typed,
/// then that command's own argument once the word is settled.
///
/// The commands with arguments worth completing are the ones whose argument is
/// a name out of a known set: `/model` against the config's models, `/resume`
/// against the workspace's saved sessions, `/worktree` against the
/// repository's checkouts. A prompt is prose and a focus phrase is prose;
/// guessing at either is worse than leaving it alone.
///
/// The last two are asked for through a call rather than handed over, because
/// every line but theirs is completed without them: reading the workspace's
/// sessions is a walk of every transcript in it and reading the checkouts is a
/// `git worktree list`, and a frame that offers neither should pay neither.
pub fn complete<'a>(
    line: &str,
    commands: &[Command],
    models: &[Choice],
    sessions: impl Fn() -> &'a [ResumeChoice],
    worktrees: impl Fn() -> &'a [Choice],
) -> Vec<Candidate> {
    if !line.starts_with('/') {
        return Vec::new();
    }
    let Some((word, rest)) = line.split_once(char::is_whitespace) else {
        // The exact word stays in the list. Dropping it would leave only the
        // longer `/news` when `/new` is typed in full, and Tab would hand the
        // line to the wrong command.
        return commands
            .iter()
            .filter(|c| c.word.starts_with(line))
            .map(|c| Candidate {
                show: format!("{} {}", c.word, c.args).trim_end().to_string(),
                line: c.word.to_string(),
                help: c.help.to_string(),
                more: !c.args.is_empty(),
            })
            .collect();
    };
    let typed = rest.trim_start();
    match word {
        // A second word means the model name is settled and something else is
        // being typed. There is no third thing to offer.
        "/model" if typed.contains(char::is_whitespace) => Vec::new(),
        "/model" => models
            .iter()
            .filter(|m| m.name.starts_with(typed) && m.name != typed)
            .map(|m| Candidate {
                show: m.name.clone(),
                line: format!("/model {}", m.name),
                help: m.note.clone(),
                more: false,
            })
            .collect(),
        // `rm` marks what follows it for removal; a bare `rm` still means the
        // name of a worktree, so nothing is offered until the space says.
        "/worktree" if typed == "rm" => Vec::new(),
        "/worktree" if typed.starts_with("rm ") => {
            let arg = typed["rm ".len()..].trim_start();
            worktree_candidates(worktrees(), "/worktree rm ", arg)
        }
        // A name may hold a slash (`feat/one`), so unlike a model it is not
        // settled by the first word — only whitespace after it settles it.
        "/worktree" if typed.contains(char::is_whitespace) => Vec::new(),
        "/worktree" => worktree_candidates(worktrees(), "/worktree ", typed),
        // A first question is a whole sentence, so the argument may keep several words.
        "/resume" => sessions()
            .iter()
            .filter(|s| {
                !s.prompt.is_empty() && (s.prompt.starts_with(typed) || s.id.starts_with(typed))
            })
            .map(|s| Candidate {
                show: crate::render::clip(&s.prompt, RESUME_WIDTH),
                line: format!("/resume {}", s.id),
                help: ago(s.created),
                more: false,
            })
            .collect(),
        _ => Vec::new(),
    }
}

// How much of a session's first prompt a list row or completion shows.
const RESUME_WIDTH: usize = 60;

/// A session and everything that outlives any one turn of it.
///
/// Both surfaces hold one of these and differ only in how they read a line and
/// where they put what comes back.
pub struct Repl {
    pub store: Store,
    /// Held so `/keys` can show what is actually in force, overrides included.
    pub keys: std::sync::Arc<crate::keys::Keys>,
    /// The config in force, as opposed to the one on disk. `/model` picks from
    /// this, so a switch cannot quietly apply an edit `/reload` has not.
    pub config: std::sync::Arc<crate::config::Config>,
    /// The command line, kept because it outranks the config and so has to be
    /// re-applied over every reload.
    pub args: std::sync::Arc<crate::Args>,
    /// What a slash answers to, built-ins and skills together. Rebuilt by
    /// `/reload`, because a skill can appear between one turn and the next.
    ///
    /// Shared rather than copied, like the key map beside it: the terminal
    /// holds the same table to complete against and re-reads it whenever this
    /// one is replaced.
    pub commands: std::sync::Arc<Vec<Command>>,
    /// The config tree as last read from disk. `/settings` edits a copy of
    /// this tree; `/reload` replaces it.
    pub file: toml::Value,
    /// What the settings panel has claimed this run, by path. Replayed over
    /// every reload so the claimed values keep winning over the file.
    pub claimed: BTreeMap<String, toml::Value>,
    /// Every checkout open in this run, in the order they were opened. The
    /// main one is first, because that is where a run starts.
    pub lanes: Vec<Lane>,
    /// Which of them is in front. The surface shows one at a time.
    pub current: usize,
}

impl Repl {
    // Put the lane in front's key map and command table in force. A skill
    // belongs to one tree and not another, and so does a rebound key;
    // leaving the last lane's in place had this one answering to another
    // tree's.
    fn in_force(&mut self) {
        self.keys = self.lane().keys.clone();
        self.commands = self.lane().commands.clone();
    }

    /// The checkout in front. Indexing is safe by construction: `lanes` is
    /// never empty, so `current` always names one.
    pub fn lane(&self) -> &Lane {
        &self.lanes[self.current]
    }

    pub fn remove_lane(&mut self, at: usize) -> Lane {
        let lane = self.lanes.remove(at);
        if at < self.current || self.current >= self.lanes.len() {
            self.current = self.current.saturating_sub(1);
        }
        lane
    }

    // Where a subagent started in this lane files what it did. Root and model
    // vary — a `/worktree` moves one, a `/model` the other — and the rest
    // never does.
    fn home(
        &self,
        root: std::path::PathBuf,
        model: String,
    ) -> std::sync::Arc<dyn agent::task::Home> {
        crate::subagent::Filed::armed(self.store.clone(), root, model)
    }

    pub fn lane_mut(&mut self) -> &mut Lane {
        &mut self.lanes[self.current]
    }
}

impl Repl {
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
        let tree = match crate::config::load_tree(self.args.config.as_deref()) {
            Ok(t) => t,
            Err(e) => return vec![format!("nothing reloaded — {}", refused("reload", e))],
        };
        self.file = tree;
        let mut said = self.rebuild();
        // Name any claim that still shadows a line the file just changed.
        for path in self.claimed.keys() {
            if let Ok(old) = crate::settings::get(&self.file, path)
                && old != &self.claimed[path]
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
        let project = match crate::config::load_project(&root) {
            Ok(p) => p,
            Err(e) => return failed(e),
        };
        let mut resolved = match crate::resolve(
            &self.args,
            &self.lane().ctx.workspace,
            &config,
            &project,
            &self.claimed,
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
            home,
            standing: &resolved.standing,
            task_max_turns: resolved.max_turns,
            task_deadline: resolved.task_deadline,
        });
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
            crate::config::Origin::Command,
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

    // The file tree with the claimed overrides on top — what the config is
    // computed from.
    fn effective(&self) -> Result<toml::Value, anyhow::Error> {
        let mut tree = self.file.clone();
        for (path, value) in &self.claimed {
            crate::settings::put(&mut tree, path, value.clone())?;
        }
        Ok(tree)
    }

    // The file's rows with the session's claims on top — what the panel
    // shows and the read-only list prints. Path by path rather than one
    // overlaid tree, so a claim the file can no longer address (an ancestor
    // the file has turned into a non-table) still answers, with the file's
    // own value beside it for the mark.
    pub fn setting_rows(&self) -> Vec<crate::settings::SettingRow> {
        let mut rows: BTreeMap<String, String> =
            crate::settings::leaves(&self.file).into_iter().collect();
        for (path, claimed) in &self.claimed {
            rows.insert(path.clone(), crate::settings::render(claimed));
        }
        rows.into_iter()
            .map(|(path, value)| {
                let claimed = self.claimed.get(&path);
                let file = crate::settings::get(&self.file, &path).ok();
                crate::settings::SettingRow {
                    path,
                    value,
                    changed: claimed.is_some() && claimed != file,
                }
            })
            .collect()
    }

    // The same, saying why when nothing could be adopted.
    fn rebuilt(&mut self) -> Result<Vec<String>, String> {
        let tree = self.effective().map_err(|e| refused("settings", e))?;
        let mut config = match crate::config::Config::deserialize(tree) {
            Ok(c) => c,
            Err(e) => return Err(refused("settings", anyhow::anyhow!(e))),
        };
        config.apply_env_unclaimed(&self.claimed);
        self.adopt(config)
    }

    /// Take a value into the session: try the write on a scratch tree first,
    /// so a bad value touches nothing, then record it as a claim and rebuild.
    /// The panel's edit line answers through here, so a refusal comes back
    /// named, to be shown beside the edit that earned it.
    pub fn edit(&mut self, path: &str, raw: &str) -> Result<Vec<String>, String> {
        let raw = typed(path, raw);
        let mut scratch = match self.effective() {
            Ok(t) => t,
            Err(e) => return Err(refused("settings", e)),
        };
        let old = crate::settings::get(&scratch, path).ok().cloned();
        if let Err(e) = crate::settings::set(&mut scratch, path, &raw) {
            return Err(refused("settings", e));
        }
        let new = crate::settings::get(&scratch, path).unwrap().clone();
        // Validate by deserializing the scratch tree, so a bad value never
        // reaches the running config.
        if let Err(e) = crate::config::Config::deserialize(scratch) {
            return Err(refused("settings", anyhow::anyhow!(e)));
        }
        self.claimed.insert(path.to_string(), new);
        let mut said = self.rebuild();
        let old_shown = match &old {
            Some(v) => mask_secret(path, &crate::settings::render(v)),
            None => "<unset>".to_string(),
        };
        said.push(format!(
            "{path}: {old_shown} → {} (session only)",
            mask_secret(path, &crate::settings::render(&self.claimed[path]))
        ));
        Ok(said)
    }

    /// The panel's r: the file's value takes the session back. No claim on
    /// the path means the file is already in force and there is nothing to
    /// say.
    pub fn revert(&mut self, path: &str) -> Vec<String> {
        if self.claimed.remove(path).is_none() {
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
        if !self.claimed.contains_key(path) {
            return Ok(vec![format!("{path}: the session and the file agree")]);
        }
        let tree = self.effective().map_err(|e| format!("{e:#}"))?;
        let value = crate::settings::get(&tree, path)
            .map_err(|e| format!("{e:#}"))?
            .clone();
        let file = self
            .args
            .config
            .as_deref()
            .map(std::path::PathBuf::from)
            .or_else(crate::config::global_path)
            .ok_or_else(|| "no settings file to write".to_string())?;
        crate::config::write(&file, path, value.clone()).map_err(|e| format!("{e:#}"))?;
        self.claimed.remove(path);
        self.file =
            crate::config::load_tree(self.args.config.as_deref()).map_err(|e| format!("{e:#}"))?;
        let mut said = self.rebuild();
        said.push(format!(
            "{path} = {} — written to the file",
            mask_secret(path, &crate::settings::render(&value))
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
        let dialled = match crate::dial(
            &self.args,
            &self.config,
            name,
            crate::config::Origin::Command,
        ) {
            Ok(d) => d,
            Err(e) => {
                let held = self.lane_mut().agent.spec.model.clone();
                return vec![format!("still on {held} — {}", refused("switch", e))];
            }
        };
        // Compared after resolving, not before: `find` accepts a model's
        // `wire_id` as well as its table name, so the name typed and the id it
        // lands on need not be the same string. Comparing the typed one would
        // re-dial the model already running and then announce a reasoning
        // demotion that never happened.
        if dialled.spec.model == self.lane_mut().agent.spec.model {
            return vec![format!("already on {}", self.lane_mut().agent.spec.model)];
        }
        let mut said: Vec<String> = dialled.warning.into_iter().chain(dialled.notes).collect();
        let spec = &dialled.spec;
        said.push(format!(
            "now on {}{}{}",
            spec.model,
            icons::PART_SEP,
            summary(spec.format.name(), spec.context_window, &spec.pricing)
        ));
        // An absent transcript is one a run has, and it is writing this
        // model's reasoning into it as we speak — so say it either way.
        if self
            .lane_mut()
            .session
            .as_ref()
            .is_none_or(carries_reasoning)
        {
            said.push(demotion(spec.replay_thinking).into());
        }
        tracing::info!(
            target: "pi::session",
            from = %self.lane_mut().agent.spec.model,
            to = %spec.model,
            format = spec.format.name(),
            context_window = spec.context_window,
            "model switched"
        );
        self.retarget(dialled.transport, dialled.spec);
        said
    }

    // Point this lane at a new endpoint, and rebuild the subagent behind it.
    //
    // `Task` holds a snapshot of the agent it was built from, so a retarget
    // that stopped at the lane would leave the child on the old provider —
    // with the old key — while the status line named the new model.
    fn retarget(
        &mut self,
        transport: std::sync::Arc<dyn brain::Transport>,
        spec: brain::ModelSpec,
    ) {
        let home = self.home(
            self.lane().ctx.workspace.root().to_path_buf(),
            spec.model.clone(),
        );
        let standing = self.lane().standing.clone();
        let ag = std::sync::Arc::make_mut(&mut self.lane_mut().agent);
        ag.retarget(transport, spec);
        ag.hang(home, &standing);
    }

    // What `/model` on its own shows.
    fn listing(&self) -> Vec<String> {
        let here = &self.lane().agent.spec.model;
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

    /// Save the transcript. Called after every turn: an interrupted one is
    /// exactly the one worth keeping.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_lane(self.current)
    }

    /// The same for a lane that is not in front: a run that ended out of sight
    /// still has to reach disk, and it is not the screen's turn that decides.
    pub fn save_lane(&self, at: usize) -> anyhow::Result<()> {
        // Away with a run, which saves it itself on the way back.
        let Some(lane) = self.lanes.get(at) else {
            return Ok(());
        };
        let Some(session) = &lane.session else {
            return Ok(());
        };
        self.store.save(
            &lane.id,
            lane.ctx.workspace.root(),
            &lane.agent.spec.model,
            lane.name.as_deref(),
            lane.created,
            session,
        )?;
        Ok(())
    }

    /// Shrink the transcript, or None when there was nothing to shrink — and
    /// likewise when a run has it, which is why `/compact` is refused then.
    ///
    /// Here rather than at each surface: both asked the agent directly, and
    /// both had to reach past the lane for the session to do it.
    pub async fn compact_now(
        &mut self,
        focus: Option<&str>,
    ) -> Option<(agent::compact::Report, Totals)> {
        // One borrow of the lane, two of its fields: they are disjoint, and
        // asking twice would not be.
        let lane = self.lane_mut();
        let session = lane.session.as_mut()?;
        lane.agent.compact_now(session, focus).await
    }

    /// What the transcript occupies now, for the line that says why there was
    /// nothing to compact. Zero while a run has it.
    pub fn tokens_now(&self) -> usize {
        self.tokens_now_at(self.current)
    }

    /// The same for a named lane: a compaction that finishes after the screen
    /// has moved on still has to say what it found.
    pub fn tokens_now_at(&self, at: usize) -> usize {
        let Some(lane) = self.lanes.get(at) else {
            return 0;
        };
        lane.session.as_ref().map_or(0, |s| {
            brain::estimate::tokens(&s.context(), &lane.agent.spec)
        })
    }

    /// Rewind the conversation to an entry and write the shorter transcript
    /// back.
    ///
    /// The entry decides which of the two it is — the caller cannot get the
    /// pairing wrong, and a menu row that went stale against the transcript
    /// falls through to removing nothing.
    pub fn rewind_to(&mut self, entry: agent::session::EntryId) -> anyhow::Result<Rewound> {
        // A rewind is refused while a run has the transcript, so this is the
        // idle path; without it there is nothing to go back through.
        let Some(session) = &mut self.lane_mut().session else {
            return Ok(Rewound::Nothing);
        };
        let unsent = session.unsent_text(entry);
        let removed = match unsent {
            Some(_) => session.rollback_before(entry),
            None => session.rollback_to(entry),
        };
        if removed == 0 {
            return Ok(Rewound::Nothing);
        }
        self.save()?;
        Ok(match unsent {
            Some(text) => Rewound::Unsent(text),
            None => Rewound::Kept,
        })
    }
}

// Enough about a model to choose between them: who serves it, how much it
// holds, and what it costs where that is known.
//
// Takes the three pieces rather than a config entry, because the running model
// may never have been one — a name passed through with default numbers has no
// entry to read.
fn summary(format: &str, window: u32, p: &brain::model::Pricing) -> String {
    let mut parts = vec![format.to_string(), format!("{}k", window / 1000)];
    if p.input_per_mtok > 0.0 || p.output_per_mtok > 0.0 {
        parts.push(format!(
            "${:.2}/${:.2} per Mtok",
            p.input_per_mtok, p.output_per_mtok
        ));
    }
    parts.join(icons::PART_SEP)
}

// What becomes of the transcript's reasoning once another model is reading it.
//
// Only ever asked about a model that did not write it — the origin recorded on
// each block cannot match after a switch — so the signed path is out and one of
// these three is what the transport will do with it.
fn demotion(replay: brain::model::ReplayThinking) -> &'static str {
    use brain::model::ReplayThinking as R;
    match replay {
        R::Tagged => "reasoning from the earlier turns replays wrapped in <think> tags",
        R::Off => "reasoning from the earlier turns is dropped rather than replayed",
    }
}

// Whether the transcript holds any prior-turn reasoning at all.
//
// Worth saying at a switch: it is the one part of the history that does not
// survive intact, and a model that suddenly reads its own earlier thinking as
// quoted prose is otherwise an unexplained change in tone.
fn carries_reasoning(session: &agent::session::Session) -> bool {
    // The view, not every entry: what compaction has already dropped is not
    // going to reach the new model in any form, demoted or otherwise.
    session.view().iter().any(|s| {
        s.entry().blocks().is_some_and(|bs| {
            bs.iter()
                .any(|b| matches!(b, brain::message::AssistantContent::Reasoning(_)))
        })
    })
}

/// What an input asked the loop to do, whatever it arrived as.
///
/// One vocabulary for the keyboard, the phone and a typed line, so the same
/// intent gets the same answer however it was expressed.
#[derive(Debug, PartialEq, Eq)]
pub enum Intent {
    // Nothing for the loop: the press changed only what `Ui` owns.
    None,
    // A line the user submitted, not yet read. The one raw variant, because
    // the screen echoes the text and the queue holds it — parsing it at the
    // door would throw away what both of them need. `read` says what it is.
    Submit(String),

    // What `read` makes of a submitted line.
    // Prose for the model.
    Prompt(String),
    // What `!` named.
    Bash(String),
    Help,
    Keys,
    Status,
    Reload,
    Name(String),
    // The session to switch to, or empty to list what there is.
    Resume(String),
    // Everything after the word focuses the summary.
    Compact(String),
    // The name to move to, or empty to list what there is.
    Model(String),
    // The name to work in, or empty to list what there is.
    Worktree(String),
    // Bare `/settings`. The panel is the whole surface; the line verbs are
    // gone, and an argument after the word is refused.
    Settings(String),
    // The wechat verb: "" = status, "on" = connect, "off" = disconnect.
    Wechat(String),
    // What to run over and over, or empty to stop the loop in force.
    Loop(String),
    // One round of a loop, put back through the door by the surface. Apart
    // from `Submit` only so the turn it starts can be told from a line typed
    // between rounds — `read` never answers it. `note` is the loop's word to
    // the model, filed as machine prose rather than glued to the goal, which
    // must stay what `read` would parse.
    LoopRound {
        goal: String,
        note: String,
        // Which automatic round this is: `None` for the loop's first — that
        // one is the `/loop goal` line itself — `Some(n)` for n ≥ 2.
        // Recorded on the ask the round opens.
        round: Option<u64>,
    },
    // Not a built-in word. It may name a skill and it may name nothing; the
    // command table settles that, and `read` does not have it.
    Other {
        word: String,
        args: String,
    },
    // `/new`, and `ctrl+l` twice: a fresh session, the old one kept on disk,
    // and the screen rebuilt from the empty one. One variant, because they
    // are one intent however it was expressed.
    New,
    // Only a key can ask for these: there is no line that says them.
    // The rewind selector wants everywhere the session can go back to.
    OpenRewind,
    // A row chosen from the rewind selector: the conversation rewinds there,
    // and what the row was decides whether it is kept or unsent.
    Rewind(agent::session::EntryId),
    // The settings panel's edit line kept a value: the session takes it, and
    // the file does not — `SettingWrite` is what moves it to the file.
    SettingEdit(String, String),
    // The panel's space: the session value replaces the file's line.
    SettingWrite(String),
    // The panel's r: the file's value takes the session back.
    SettingRevert(String),
    // The line being typed wants `$EDITOR`. The surface's own: the editor
    // takes the terminal, which only the surface knows how to give away.
    EditExternally,
    // Esc caught a prompt on its way out: stop the run, then unsend it.
    Unsend,
    // Leave now — `/exit`, `/quit`, `ctrl+d`, a double `ctrl+c`. One intent,
    // so the four of them cannot answer differently.
    Quit,

    // A key or the phone.
    Interrupt,
}

/// What a line may do while a turn is in flight.
///
/// Read off the parsed command rather than off the `Step` it produces:
/// `command` has already had its effect by the time it hands one back, so a
/// `Step` can say what happened but never whether it should have.
#[derive(Debug)]
pub enum Fate {
    // Touches nothing the run is standing on.
    Now,
    // Goes to the model, or needs the surface free, so it waits.
    Queued,
    // Prose, while a run is working: it goes to the run rather than waiting
    // for it, and is read at the run's next turn boundary.
    //
    // Carries the text because answering took a read of the line — `Submit`
    // cannot say what it is without one — and reading it a second time at the
    // door is how two readings drift apart.
    Steered(String),
    // Would move what the run stands on. Says this rather than doing it.
    Refused(&'static str),
}

impl Intent {
    /// Exhaustive on purpose, with no catch-all arm: an intent added without an
    /// answer here should fail to compile rather than default to one.
    ///
    /// What it cannot check is an existing arm's body: `Reload` and `Model` are
    /// `Now` because they write through `Arc::make_mut`, not because anything
    /// says so. An arm that starts reading `lane.session` breaks this quietly —
    /// the session is away for the length of a run.
    pub fn fate(&self) -> Fate {
        match self {
            // Answered from the config, the key map or the view's own tally
            // — none of which the run is holding.
            Intent::Help | Intent::Keys | Intent::Status | Intent::Name(_) => Fate::Now,
            Intent::Reload | Intent::Model(_) => Fate::Now,
            // Bare, these only list what there is.
            Intent::Resume(name) | Intent::Worktree(name) if name.trim().is_empty() => Fate::Now,
            Intent::Resume(_) => Fate::Refused(
                "/resume would replace the transcript this run is writing — esc first",
            ),
            // A lane of its own to move to, and the one being left keeps
            // working in the tree it was already in.
            Intent::Worktree(_) => Fate::Now,
            Intent::New => {
                Fate::Refused("/new would replace the transcript this run is writing — esc first")
            }
            Intent::Compact(_) => {
                Fate::Refused("/compact rewrites the transcript this run is writing — esc first")
            }
            // An argument is a refusal, and a refusal answers now; bare opens
            // a panel, which wants the surface to itself.
            Intent::Settings(rest) if !rest.trim().is_empty() => Fate::Now,
            Intent::Settings(_) => Fate::Queued,
            Intent::Wechat(_) => Fate::Now,
            Intent::Other { .. } => Fate::Queued,
            // Arms the lane and submits its first round like a typed line;
            // both want the lane free.
            Intent::Loop(_) | Intent::LoopRound { .. } => Fate::Queued,
            // Prose reaches the run that is already talking to the model:
            // waiting for it is what makes a correction arrive too late to be
            // one.
            Intent::Prompt(text) => Fate::Steered(text.clone()),
            // A `!` files its result in the transcript, which the run has.
            Intent::Bash(_) => Fate::Queued,
            // The raw variant: its fate is the fate of what it turns out to be.
            // `read` never answers `Submit`, so this ends.
            Intent::Submit(line) => read(line).fate(),
            // Both reach for the transcript, and the run is holding it.
            Intent::OpenRewind | Intent::Rewind(_) => {
                Fate::Refused("rewinding needs the transcript this run is writing — esc first")
            }
            // Claiming a value and writing one to the file both rebuild
            // through `Arc::make_mut`, so a run in flight keeps the agent it
            // started on.
            Intent::SettingEdit(..) | Intent::SettingWrite(_) | Intent::SettingRevert(_) => {
                Fate::Now
            }
            // The input line is the surface's, not the transcript's: a run in
            // flight is writing the second and never reads the first.
            Intent::EditExternally => Fate::Now,
            // Guarded where they land rather than here: `stop_current` acts
            // only on a lane that is actually running.
            Intent::Interrupt | Intent::Unsend => Fate::Now,
            // Leaving is never refused: a hung run must not trap the user.
            Intent::Quit => Fate::Now,
            Intent::None => Fate::Now,
        }
    }
}

// Slash commands are recognized before anything reaches the model, so a line
// that merely starts with a slash never becomes a prompt by accident.
// A command that changed nothing, said once to the user and once to the
// journal. Nothing-happened is the hardest kind of bug to read back: the
// terminal has scrolled and the config on disk is whatever it is now.
fn refused(what: &str, e: anyhow::Error) -> String {
    let detail = format!("{e:#}");
    tracing::warn!(target: "pi::session", command = what, error = %detail, "refused");
    detail
}

fn typed<'a>(path: &str, raw: &'a str) -> Cow<'a, str> {
    match path {
        "base_url" => Cow::Owned(crate::config::expand_base_url(raw)),
        _ => Cow::Borrowed(raw),
    }
}

// A skill command as a message the user could have typed, or why it could not
// be read.
//
// The body goes in whole rather than as an instruction to go and fetch it:
// `/commit` says the user has already chosen those instructions, and a model
// that must call the `skill` tool to learn what it just agreed to has spent a
// turn on a decision that was made before it was asked.
fn expanded(skill: &Skill, args: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(skill.dir.join("SKILL.md")).map_err(|e| {
        let why = refused(&skill.name, anyhow::anyhow!("{}: {e}", skill.dir.display()));
        format!("cannot run {} — {why}", skill.name)
    })?;
    let mut out = format!(
        "Run the `{}` skill. Its instructions follow.\n\n{}",
        skill.name,
        tools::skill::instructions(skill, &text)
    );
    if !args.is_empty() {
        // Below the instructions, so the skill is read as the standing order
        // and this as what it is being applied to.
        out.push_str(&format!("\n---\n{args}\n"));
    }
    tracing::info!(
        target: "pi::session",
        skill = %skill.name,
        bytes = out.len(),
        args = !args.is_empty(),
        "skill invoked"
    );
    Ok(out)
}

// The skill a word names, if it names one. A built-in never reaches here —
// `parse` has already turned those into their own variants.
fn skill_for<'a>(commands: &'a [Command], word: &str) -> Option<&'a Skill> {
    match &commands.iter().find(|c| c.word.as_ref() == word)?.source {
        Source::Skill(skill) => Some(skill),
        Source::Builtin => None,
    }
}

/// Whether `line` comes back with Up: what the user said, and a skill — a
/// skill command is a prompt wearing a slash. Built-ins are operations
/// rather than words to re-say, and a word pi does not know is nothing.
pub(crate) fn recallable(line: &str, commands: &[Command]) -> bool {
    match line.split_whitespace().next() {
        Some(word) if word.starts_with('/') => skill_for(commands, word).is_some(),
        _ => true,
    }
}

// A word `parse` did not know: a skill to run, or a typo to name.
fn dispatch(commands: &[Command], word: &str, args: &str) -> Step {
    let Some(skill) = skill_for(commands, word) else {
        return Step::Flash(format!("unknown command {word} — /help lists them"));
    };
    match expanded(skill, args) {
        Ok(send) => Step::Prompt {
            typed: Some(format!("/{} {args}", skill.name).trim_end().to_string()),
            send,
        },
        Err(why) => lines(why),
    }
}

/// What a one-shot prompt turns into when it names a skill.
///
/// `pi "/commit fix the tests"` means at the command line what it means at the
/// terminal. That is the whole guarantee, and it is deliberately narrower than
/// the terminal's: everything else that starts with a slash is left alone.
///
/// The built-ins are operations on a session, and a run that answers once has
/// no session for them to operate on. A word that names nothing is not a typo
/// to be refused either, because here the argument is a prompt rather than a
/// line at a prompt — `pi "/usr/bin is missing"` and `pi "/2 of the tests
/// fail"` are prose, and refusing them to catch `/comit` trades a recoverable
/// mistake for an unrecoverable one. The model can ask what `/comit` meant; a
/// user whose sentence was rejected has to reword it.
pub fn expand(commands: &[Command], line: &str) -> Option<Result<String, String>> {
    let Intent::Other { word, args } = read(line) else {
        return None;
    };
    Some(expanded(skill_for(commands, &word)?, &args))
}

// What a line starting with `!` asks to run, when it names a command.
//
// `!` alone is prose (a prompt, like any other line); `!cmd` runs `cmd`.
// `!!cmd` keeps its second bang: in shell grammar `! cmd` negates the exit
// code, which is what a non-interactive shell will do with it.
fn bash_command(line: &str) -> Option<&str> {
    let cmd = line.strip_prefix('!')?.trim();
    (!cmd.is_empty()).then_some(cmd)
}

/// What a submitted line is asking for. Total on purpose: every line means
/// something, and a line naming no command is prose for the model.
///
/// `!` is read before the slash words, because a shell command is not one.
pub fn read(line: &str) -> Intent {
    if let Some(command) = bash_command(line) {
        return Intent::Bash(command.to_string());
    }
    let Some(word) = line.split_whitespace().next() else {
        return Intent::Prompt(line.to_string());
    };
    if !word.starts_with('/') {
        return Intent::Prompt(line.to_string());
    }
    match word {
        "/exit" | "/quit" => Intent::Quit,
        "/help" => Intent::Help,
        "/new" => Intent::New,
        "/resume" => Intent::Resume(rest(line)),
        "/keys" => Intent::Keys,
        "/reload" => Intent::Reload,
        "/status" => Intent::Status,
        "/name" => Intent::Name(rest(line)),
        "/compact" => Intent::Compact(rest(line)),
        "/model" => Intent::Model(rest(line)),
        "/worktree" => Intent::Worktree(rest(line)),
        "/wechat" => Intent::Wechat(rest(line)),
        "/loop" => Intent::Loop(rest(line)),
        "/settings" => Intent::Settings(rest(line)),
        other => Intent::Other {
            word: other.to_string(),
            args: rest(line),
        },
    }
}

// Whatever followed the command word.
fn rest(line: &str) -> String {
    line.trim_start()
        .split_once(char::is_whitespace)
        .map_or(String::new(), |(_, r)| r.trim().to_string())
}

/// What a rewind did. Which one it is says whether the entry was unsent or
/// kept, rather than leaving that to be read off whether a string was there.
pub enum Rewound {
    // The id named nothing the transcript still holds.
    Nothing,
    // The entry stayed, and what followed it went.
    Kept,
    // The entry went too, and its text belongs back in the editor.
    Unsent(String),
}

#[derive(Debug)]
pub enum Step {
    // One line whose whole content is "nothing happened": a verb that is not
    // one, a command refused, a checkout you are already in. No state moved
    // and there is no detail to come back to, so a minute later the line says
    // nothing the screen does not already show.
    //
    // The surface shows it on the bar for a moment and keeps it out of the
    // scrollback — see `Ui::flash`. An error carrying detail is the other
    // side of that line and stays in the transcript.
    Flash(String),
    // A `!` command to run. The surface runs and records it, because only it
    // can await; `run_bash` does the work and `record_bash` files it.
    Bash(String),
    // What to send, and — when a skill expanded into it — the line that was
    // typed. `rewind_nodes()` reads the second: a rewind menu offering four
    // thousand characters of `SKILL.md` is offering the wrong thing, and so
    // is a session named after one.
    Prompt { send: String, typed: Option<String> },
    // Needs the network, so the surface runs it and reports.
    Compact(Option<String>),
    // Starts or stops the wechat bridge. Needs the network, so the surface
    // runs it and reports — the same rule as `Compact`.
    Wechat(WechatCmd),
    // Dealt with here; these lines are what there is to show for it. Returned
    // rather than printed because one surface prints and the other paints.
    Handled(Vec<String>),
    // The session was replaced — a `/new` or a `/resume` — so the surface
    // has to rebuild its view from the new one, not just show the lines.
    Swap(Vec<String>),
    // The set of worktrees changed under the surface — a removal — so it
    // shows the lines and forgets the cached list, which would keep naming
    // the checkout that just went.
    Worktrees(Vec<String>),
    Quit,
}

/// What `/wechat` asks the surface to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WechatCmd {
    Status,
    On,
    Off,
}

impl Repl {
    // What this run stands on, in one place: the tail of the system prompt as
    // the model receives it, the two files a person opens when a run goes
    // wrong, and what the session has spent. The instruction files are named
    // rather than quoted — `standing` carries them whole, and their content is
    // in the files themselves.
    //
    // The spend is the session's — the status lines carry the run's own — read
    // live from the lane's tally rather than the lane's settled totals, so a run
    // under way is counted rather than waiting for it to end.
    fn status_lines(&self) -> Vec<String> {
        let lane = self.lane();
        let mut out = standing_head(&lane.standing);
        if !lane.context.is_empty() {
            out.push("context:".into());
            out.extend(lane.context.iter().map(|c| format!("- {c}")));
        }
        out.push(match crate::journal::path() {
            Some(p) => format!("journal: {}", p.display()),
            None => "journal: not recording — PI_LOG is off, or it would not open".into(),
        });
        out.push(format!(
            "session: {}",
            self.store
                .path_of(lane.ctx.workspace.root(), &lane.id)
                .display()
        ));
        // Left out until something has been spent: a session nothing has been
        // asked of yet has no figure, and a row of dashes is not one.
        let spent = lane.view.session_spend();
        if spent != Totals::default() {
            out.push(format!("spent: {}", crate::render::spent(&spent)));
        }
        out
    }
}

// What the system prompt's tail says about the run, which is all of it up to
// the first instruction file. Split on the tag rather than counting parts, so
// that a field added to the head shows up here without being told to; the
// files are named separately because `standing` carries them whole.
fn standing_head(standing: &str) -> Vec<String> {
    standing
        .split("<instructions")
        .next()
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

fn lines(text: impl Into<String>) -> Step {
    Step::Handled(text.into().lines().map(str::to_string).collect())
}

// How long ago a transcript was last saved, in human terms.
fn ago(secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ago = now.saturating_sub(secs);
    if ago < 60 {
        "just now".into()
    } else if ago < 3600 {
        format!("{}m ago", ago / 60)
    } else if ago < 86400 {
        format!("{}h ago", ago / 3600)
    } else {
        format!("{}d ago", ago / 86400)
    }
}

impl Repl {
    /// Carry out an intent, or say what the surface must do to carry it out.
    ///
    /// Exhaustive with no catch-all, like `Intent::fate`: the arms a surface
    /// answers for itself are named rather than swept up, so a new intent has
    /// to say which side of that line it falls on.
    pub fn run(&mut self, intent: Intent) -> Step {
        match intent {
            Intent::Bash(command) => Step::Bash(command),
            Intent::Prompt(send) => Step::Prompt { send, typed: None },
            // Unreachable by construction: `read` never makes these, and the
            // surface acts on them before it gets here. The arm earns its keep
            // anyway — it is what makes the exhaustiveness check bite, so a new
            // intent has to say which side of this line it falls on.
            Intent::Submit(_)
            | Intent::None
            | Intent::Interrupt
            | Intent::Unsend
            | Intent::OpenRewind
            | Intent::Rewind(_)
            | Intent::SettingEdit(..)
            | Intent::SettingWrite(_)
            | Intent::SettingRevert(_)
            | Intent::EditExternally => Step::Handled(Vec::new()),
            // The surface's, like `Submit`: arming a lane and re-submitting a
            // line through `read` are both things only it can do, so it takes
            // this before `run` is reached.
            Intent::Loop(_) | Intent::LoopRound { .. } => Step::Handled(Vec::new()),
            Intent::Quit => Step::Quit,
            Intent::Help => Step::Handled(help(&self.commands)),
            Intent::Keys => Step::Handled(self.keys.listing()),
            Intent::Reload => Step::Handled(self.reload()),
            Intent::Status => Step::Handled(self.status_lines()),
            Intent::New => {
                self.fresh_session();
                Step::Swap(Vec::new())
            }
            Intent::Resume(name) => {
                if name.is_empty() {
                    Step::Handled(self.resume_listing())
                } else {
                    match self.resume(&name) {
                        Ok(said) => Step::Swap(said),
                        Err(why) => Step::Handled(vec![why]),
                    }
                }
            }
            Intent::Name(name) => {
                if name.is_empty() {
                    self.lane_mut().name = None;
                    lines(format!("{} is unnamed again", self.lane_mut().id))
                } else {
                    let said = format!("{} is now “{name}”", self.lane_mut().id);
                    self.lane_mut().name = Some(name);
                    lines(said)
                }
            }
            Intent::Compact(focus) => Step::Compact(Some(focus).filter(|f| !f.is_empty())),
            Intent::Model(name) => Step::Handled(if name.is_empty() {
                self.listing()
            } else {
                self.switch(&name)
            }),
            Intent::Worktree(name) => {
                if name.is_empty() {
                    Step::Handled(self.worktree_listing())
                } else {
                    // `rm` + a name removes the tree; a bare `rm` still names
                    // a tree of its own, so only the two-word form is the verb.
                    let step = match name.split_once(char::is_whitespace) {
                        Some(("rm", name)) if !name.trim().is_empty() => {
                            self.remove_worktree(name.trim())
                        }
                        _ => self.enter_worktree(&name),
                    };
                    match step {
                        Ok(step) => step,
                        Err(why) => Step::Handled(vec![why]),
                    }
                }
            }
            Intent::Other { word, args } => dispatch(&self.commands, &word, &args),
            Intent::Wechat(rest) => match rest.trim() {
                "" => Step::Wechat(WechatCmd::Status),
                "on" => Step::Wechat(WechatCmd::On),
                "off" => Step::Wechat(WechatCmd::Off),
                other => Step::Flash(format!("unknown /wechat verb `{other}` — bare, on or off")),
            },
            Intent::Settings(rest) => self.settings(&rest),
        }
    }

    // `/settings`. The panel is the whole surface: bare opens it, and anything
    // after the word is refused rather than half-remembered as a verb.
    fn settings(&mut self, rest: &str) -> Step {
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
                line.push_str(&format!(" {}", crate::icons::CHANGED_MARK));
                if let Ok(file) = crate::settings::get(&self.file, &row.path) {
                    let file = mask_secret(&row.path, &crate::settings::render(file));
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

    // Become the session this id names: the stamp that dates it, the journal
    // it writes to, and the namespace its spills are filed under.
    //
    // One place because the id and the stamp always travel together and the
    // two callers each set what the other did not — `created` was the one
    // that got missed, and a resumed session was then re-dated on its next
    // save with the stamp of the session it had just left.
    fn becomes(&mut self, id: String, created: u64) {
        self.lane_mut().id = id;
        self.lane_mut().created = created;
        // What `/status` reports is read off the view's tally, seeded from the
        // lane's totals; a new session starts both at nothing rather than the
        // one just left.
        self.lane_mut().totals = Totals::default();
        self.lane_mut().view.clear_tally();
        let id = self.lane().id.clone();
        let path = self
            .store
            .journal_path(self.lane().ctx.workspace.root(), &id);
        crate::journal::switched(&path, &id);
        // Spills are filed under the session id; a session has to own its own
        // namespace or the one before it keeps swallowing them.
        self.lane_mut().ctx = self
            .lane_mut()
            .ctx
            .clone()
            .with_session(&self.lane_mut().id);
    }

    // Drop the in-memory conversation and open a fresh session under a new
    // id. The old transcript stays on disk.
    //
    // Says nothing: the screen it is rebuilt into is empty, which is the
    // whole of the news, and the id it opened under is the surface's own
    // business — as with a resumed one.
    fn fresh_session(&mut self) {
        self.lane_mut().session = Some(Session::default());
        // A name identifies one session; carried over it would name two, which
        // is what `/name` exists to prevent.
        self.lane_mut().name = None;
        self.becomes(crate::session::new_id(), crate::session::now());
    }

    // Take a stored transcript as the running one — entries, name and id.
    // Parting with what is being left is the caller's; they differ on when.
    fn adopt_session(&mut self, stored: Stored) -> Vec<String> {
        let (id, name, created) = (stored.id.clone(), stored.name.clone(), stored.created);
        let session = stored.into_session();
        self.lane_mut().name = name;
        self.lane_mut().session = Some(session);
        self.becomes(id, created);
        // The id is a timestamp with a pid in it — nothing to read, and the
        // transcript coming back on screen already says what was resumed. A
        // name is worth a line, being what the user called it.
        match self.lane_mut().name.as_deref() {
            Some(name) => vec![format!("resumed “{name}”")],
            None => Vec::new(),
        }
    }

    // `/worktree <name>`: create or reuse a checkout of this repository and
    // move the session into it.
    //
    // Each tree keeps its own transcript rather than one transcript following
    // the move: paths in it are workspace-relative, so under another root the
    // same string names a different file, and the file locks and edit shifts
    // are keyed by absolute path. Coming back therefore resumes what was being
    // said in that tree, not an empty page.
    fn enter_worktree(&mut self, name: &str) -> Result<Step, String> {
        let from = self.lane_mut().ctx.workspace.root().to_path_buf();
        let tree = crate::worktree::enter(&from, name).map_err(|e| refused("worktree", e))?;
        // Built before the comparison: both sides are then canonical, and a
        // path git and the workspace spell differently is still one directory.
        let ws = tools::Workspace::new(&tree.path)
            .and_then(|ws| ws.with_write_roots(&self.config.write_roots))
            .map_err(|e| refused("worktree", anyhow::anyhow!("{}: {e}", tree.path.display())))?;
        if ws.root() == from {
            return Ok(Step::Flash(format!("already in {}", tree.name)));
        }
        // Against the root it belongs to, so before the move, not after. An
        // empty session — nothing said yet — has nothing to keep, and one a run
        // has is saved by the run.
        if self.lane().session.as_ref().is_some_and(|s| !s.is_empty())
            && let Err(e) = self.save()
        {
            tracing::warn!(target: "pi::session", error = %e, "the leaving session was not saved");
        }
        // Already open: the lane that holds it comes back whole. Nothing is
        // said — the screen changing, bar included, says where you are.
        if let Some(i) = self
            .lanes
            .iter()
            .position(|lane| lane.ctx.workspace.root() == ws.root())
        {
            self.current = i;
            self.in_force();
            return Ok(Step::Handled(Vec::new()));
        }
        let said = self.open_lane(ws, (!tree.main).then(|| tree.name.clone()))?;
        Ok(Step::Swap(said))
    }
    // `/worktree rm <name>`: remove the checkout `name` refers to — its
    // directory, the branch it was on, and every transcript recorded under
    // it. Git says no to a checkout with changes in it, and that refusal is
    // passed on rather than forced past.
    fn remove_worktree(&mut self, name: &str) -> Result<Step, String> {
        let from = self.lane().ctx.workspace.root().to_path_buf();
        if let Some(target) = crate::worktree::list(&from)
            .ok()
            .and_then(|trees| trees.into_iter().find(|t| !t.main && t.name == name))
        {
            let running = self.lanes.iter().enumerate().any(|(i, lane)| {
                i != self.current
                    && lane.ctx.workspace.root().starts_with(&target.path)
                    && (lane.is_running() || lane.looping.is_some())
            });
            if running {
                return Err(format!(
                    "`{name}` is running in another lane of this run — stop it first"
                ));
            }
        }
        let removed = crate::worktree::remove(&from, name).map_err(|e| refused("worktree", e))?;
        for i in (0..self.lanes.len()).rev() {
            if i != self.current
                && self.lanes[i]
                    .ctx
                    .workspace
                    .root()
                    .starts_with(&removed.path)
            {
                self.remove_lane(i);
            }
        }
        let dropped = self.store.drop_under(&removed.path);
        let mut said = vec![
            format!("removed {name}"),
            removed.path.display().to_string(),
        ];
        if let Some(branch) = removed.branch {
            said.push(format!("branch {branch} deleted"));
        }
        if let Some(note) = removed.note {
            said.push(note);
        }
        if dropped > 0 {
            said.push(format!("{dropped} session record(s) dropped"));
        }
        Ok(Step::Worktrees(said))
    }

    // Open a checkout as a lane of its own, and put it in front.
    //
    // Whole or not at all, like every other path that reads a config: a tree
    // whose config or skills will not resolve leaves the run where it was.
    fn open_lane(
        &mut self,
        ws: tools::Workspace,
        worktree: Option<String>,
    ) -> Result<Vec<String>, String> {
        let root = ws.root().to_path_buf();
        let failed = |e| format!("nothing opened — {}", refused("worktree", e));
        let project = crate::config::load_project(&root).map_err(failed)?;
        let mut resolved = crate::resolve(&self.args, &ws, &self.config, &project, &self.claimed)
            .map_err(failed)?;

        let (events, inbox) = Lane::channel();
        // The model travels; what the root decides does not. A switch changes
        // trees, and which model is answering was a decision made elsewhere.
        let home = self.home(root.clone(), self.lane().agent.spec.model.clone());
        let mut ag = (*self.lane().agent).clone();
        ag.apply(agent::Setup {
            registry: std::mem::take(&mut resolved.registry),
            system: std::mem::take(&mut resolved.system),
            tier: resolved.tier,
            effort: resolved.effort,
            home,
            standing: &resolved.standing,
            task_max_turns: resolved.max_turns,
            task_deadline: resolved.task_deadline,
        });

        // Built, not cloned from the lane being left: a `Ctx`'s tables key on
        // absolute paths in one tree, and none of that lane's describe this.
        self.lanes.push(Lane {
            token: crate::lane::next_token(),
            agent: std::sync::Arc::new(ag),
            session: Some(Session::default()),
            id: String::new(),
            created: 0,
            name: None,
            totals: Totals::default(),

            context: resolved.context,
            standing: resolved.standing,
            ctx: tools::Ctx::new(ws),
            keys: std::sync::Arc::new(resolved.keys),
            commands: std::sync::Arc::new(resolved.commands),
            worktree,
            events,
            inbox,
            pending: Vec::new(),
            looping: None,
            pending_round: None,
            turn: crate::lane::Turn::Idle,
            view: Default::default(),
        });
        self.current = self.lanes.len() - 1;
        self.in_force();

        // Asked with the root the next save will file under, so a tree is found
        // by the same key it was stored by.
        let found = self.store.latest(&root);
        Ok(match found {
            Ok(stored) => self.adopt_session(stored),
            Err(e) => {
                // Nothing recorded for this tree is the ordinary case; an
                // archive that will not load is not, and says so only here.
                tracing::debug!(target: "pi::session", error = %e, "no session to resume in this worktree");
                self.fresh_session();
                Vec::new()
            }
        })
    }

    // The checkouts `/worktree` can move to, the repository's own first, the
    // one the session is in marked.
    fn worktree_listing(&self) -> Vec<String> {
        let here = self.lane().ctx.workspace.root();
        let trees = match crate::worktree::list(here) {
            Ok(t) => t,
            Err(e) => return vec![refused("worktree", e)],
        };
        // By containment rather than equality: a run started in a subdirectory
        // is still in that checkout, and it is the one to mark.
        let at = crate::worktree::holding(&trees, here).map(|t| t.path.clone());
        let width = trees
            .iter()
            .map(|t| unicode_width::UnicodeWidthStr::width(t.name.as_str()))
            .max()
            .unwrap_or(0);
        let mut out: Vec<String> = trees
            .iter()
            .map(|t| {
                let mark = if at.as_ref() == Some(&t.path) {
                    "*"
                } else {
                    " "
                };
                let on = t.branch.as_deref().unwrap_or("detached HEAD");
                format!("{mark} {}  {on}", crate::render::pad(&t.name, width))
            })
            .collect();
        out.push(format!(
            "/worktree <name> works in one, creating it under {}/ if it is not there",
            crate::worktree::DIR
        ));
        out.push("/worktree rm <name> removes one — its checkout, sessions and branch".into());
        out
    }

    // The sessions `/resume` can switch to, newest first, the one running
    // now marked.
    fn resume_listing(&self) -> Vec<String> {
        let list = self.store.choices(self.lane().ctx.workspace.root());
        if list.is_empty() {
            return vec![
                "no sessions recorded for this workspace".into(),
                "one is saved here at the end of every turn".into(),
            ];
        }
        // What a session is known by is its first question, not its id; a
        // session with nothing said yet is not worth naming.
        let shown: Vec<(bool, String, u64)> = list
            .iter()
            .map(|s| {
                let text = if s.prompt.is_empty() {
                    "(no question)".into()
                } else {
                    crate::render::clip(&s.prompt, RESUME_WIDTH)
                };
                (s.id == self.lane().id, text, s.created)
            })
            .collect();
        let width = shown
            .iter()
            .map(|(_, t, _)| unicode_width::UnicodeWidthStr::width(t.as_str()))
            .max()
            .unwrap_or(0);
        let mut out: Vec<String> = shown
            .iter()
            .map(|(mark, text, created)| {
                format!(
                    "{} {}  {:>10}",
                    if *mark { icons::CURRENT_ITEM } else { " " },
                    crate::render::pad(text, width),
                    ago(*created)
                )
            })
            .collect();
        out.push("resume one by typing /resume and Tab".into());
        out
    }

    // Switch to a saved session: its transcript, name and id become this
    // one's, and every further turn extends it. The session being left is
    // saved first, so nothing is lost on the way out.
    //
    // The model in charge does not change — that stays the prompt's decision
    // — so reasoning a different model wrote is demoted the way it is after
    // a `/model` switch.
    fn resume(&mut self, id: &str) -> Result<Vec<String>, String> {
        // Resuming the session already running is no switch; going through
        // would only zero the totals the bar is mid-way through showing.
        if id == self.lane().id {
            return Ok(Vec::new());
        }
        // The session being left has to survive too, or /resume throws it
        // away. An empty one — just opened, nothing said — has nothing to keep.
        if self
            .lane_mut()
            .session
            .as_ref()
            .is_some_and(|s| !s.is_empty())
            && let Err(e) = self.save()
        {
            tracing::warn!(target: "pi::session", error = %e, "resume could not save the leaving session");
        }
        let stored = self.store.load(id).map_err(|e| refused("resume", e))?;
        Ok(self.adopt_session(stored))
    }
}

/// What a `!` command left behind, kept apart because the two are easy to
/// confuse: one is written to the transcript, the other drawn on the screen.
pub struct Bashed {
    /// What the model reads: the command *and* its output.
    ///
    /// Storing only the command left the rebuilt scrollback printing
    /// `Ran \`git status\`` as a prompt with the output indented under it —
    /// the live path never did that, because it had the typed line.
    pub text: String,
    /// Said on screen only — `cancelled`, or why the runner refused. There
    /// is no entry to file, so there is nothing to rebuild from either.
    pub flash: Option<String>,
}

/// Run what `!` named. Same runner, workspace and timeout as the model's own
/// `bash` tool; `ctx` carries the token that lets Esc stop it.
///
/// Free of `Repl` so the surface can spawn it: holding `&mut Repl` across the
/// await pinned the whole loop, which is what left the `!` path with an event
/// loop of its own. Recording the result is the caller's, and needs no await.
pub async fn run_bash(ctx: &tools::Ctx, command: &str) -> Bashed {
    let refused = |flash: String| Bashed {
        text: String::new(),
        flash: Some(flash),
    };
    let out = match tools::bash::Bash
        .execute(serde_json::json!({ "command": command }), ctx)
        .await
    {
        Ok(out) => out,
        Err(ToolError::Cancelled) => return refused("cancelled".into()),
        Err(e) => return refused(format!("failed to run `{command}`: {e}")),
    };
    let body = out.flatten();
    Bashed {
        // The command that ran, which rtk may have rewritten: the head the
        // model reads back is about what happened, not what was typed.
        text: format!(
            "Ran `{}`\n{}",
            out.preview(),
            if body.is_empty() {
                "(no output)"
            } else {
                &body
            }
        ),
        flash: None,
    }
}

/// The lines the screen shows for a filed `!` run: everything under the
/// `Ran …` head, minus the stream tags the model reads. The one derivation
/// for the live path and the rebuild, so the two cannot drift.
pub fn bash_said(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter(|l| !matches!(*l, "<stdout>" | "</stdout>" | "<stderr>" | "</stderr>"))
        .map(str::to_string)
        .collect()
}

impl Bashed {
    /// What the screen shows for this run: the derived lines, plus the flash
    /// for one that never ran. Everything the surface turns into notice rows.
    pub fn screen(self) -> Vec<String> {
        let mut lines = bash_said(&self.text);
        lines.extend(self.flash);
        lines
    }
}

/// File a finished `!` command in the transcript.
///
/// `text` empty means it never ran — cancelled, or the runner refused — and
/// there is nothing the model should answer with in view.
pub fn record_bash(session: &mut Session, command: &str, text: String) {
    if text.is_empty() {
        return;
    }
    session.push_bash(agent::session::Prompt {
        text,
        image: None,
        shown: Some(format!("!{command}")),
    });
}

// A secret value as a change line shows it: set or unset, never the value.
pub(crate) fn mask_secret(path: &str, value: &str) -> String {
    if journal::secret(journal::leaf(path)) {
        match value {
            "" => "<unset>".to_string(),
            _ => "<set>".to_string(),
        }
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {

    use super::{BUILTIN, Choice, Fate, Intent, Source, commands, read};
    use agent::session::{Entry, Prompt, Session};
    use tools::skills::Skill;

    // The sets behind `/resume` and `/worktree` cost a walk of every
    // transcript in the workspace and a `git worktree list`. A line that
    // completes against neither must not pay for either: the frame after every
    // turn was reading the whole bucket to redraw a menu nobody had opened.
    #[test]
    fn a_line_asks_for_the_lists_only_when_it_completes_against_one() {
        let asked = std::cell::RefCell::new(Vec::new());
        let sessions = || -> &[crate::session::ResumeChoice] {
            asked.borrow_mut().push("sessions");
            &[]
        };
        let worktrees = || -> &[Choice] {
            asked.borrow_mut().push("worktrees");
            &[]
        };
        let mut notes = Vec::new();
        let commands = commands(&[], &mut notes);
        let line = |text: &str| {
            asked.borrow_mut().clear();
            super::complete(text, &commands, &[], sessions, worktrees);
            asked.borrow().join(",")
        };

        assert_eq!(line("fix the flaky test"), "", "a prompt completes nothing");
        assert_eq!(line("/new"), "", "a command word needs no list");
        assert_eq!(line("/model "), "", "models are not one of the two");
        assert_eq!(line("/resume "), "sessions");
        assert_eq!(line("/worktree "), "worktrees");
        assert_eq!(line("/worktree rm "), "worktrees");

        // And what the call asks for is what it completes against.
        let one = [crate::session::ResumeChoice {
            id: "s1".into(),
            prompt: "fix the flaky test".into(),
            created: 0,
        }];
        let offered = super::complete("/resume f", &commands, &[], || &one[..], || &[]);
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].line, "/resume s1");
    }

    #[test]
    fn every_listed_command_actually_parses() {
        // The table drives help and completion; a word that reached the list
        // without reaching `parse` would offer to complete into nothing.
        for c in BUILTIN {
            assert!(
                !matches!(read(&c.word), Intent::Other { .. } | Intent::Prompt(_)),
                "{} is listed but does not parse",
                c.word
            );
        }
    }

    // A Repl whose file tree is the given TOML, enough for the `/settings`
    // surface to answer.
    fn core_with_file(file: &str) -> crate::repl::Repl {
        crate::repl::Repl {
            store: crate::session::Store::new(std::env::temp_dir().join("pi-settings-get-test")),
            keys: std::sync::Arc::new(crate::keys::Keys::default()),
            config: std::sync::Arc::new(crate::config::Config::default()),
            args: std::sync::Arc::new(<crate::Args as clap::Parser>::parse_from(["pi"])),
            commands: std::sync::Arc::new(Vec::new()),
            file: toml::from_str(file).unwrap(),
            claimed: Default::default(),
            lanes: vec![a_lane("s")],
            current: 0,
        }
    }

    fn claimed_base_url() -> crate::repl::Repl {
        let mut core = core_with_file(r#"base_url = "http://127.0.0.1:7896""#);
        core.claimed.insert(
            "base_url".to_string(),
            toml::Value::String("http://127.0.0.1:7897".to_string()),
        );
        core
    }

    #[test]
    fn rows_answer_path_by_path_when_the_file_breaks_under_a_claim() {
        // `models` is no longer a table, so the claim cannot be overlaid onto
        // the file — the row is still due, with the mark and its r.
        let mut core = core_with_file("model = \"flash\"\nmodels = 3");
        core.claimed
            .insert("models.flash".to_string(), toml::Value::String("m2".into()));
        let rows = core.setting_rows();
        let model = rows
            .iter()
            .find(|r| r.path == "model")
            .expect("the file's own row");
        assert!(!model.changed);
        let claimed = rows
            .iter()
            .find(|r| r.path == "models.flash")
            .expect("the claimed row");
        assert_eq!(claimed.value, "m2");
        assert!(claimed.changed, "the file has no value there to agree with");
    }

    // The panel's rows read file plus claims, so a claimed value is what the
    // panel shows — and the row is marked, the file still holding another.
    // A claim on a path the file lacks is its own row, not a patch onto a
    // value that is not there.
    #[test]
    fn panel_rows_read_the_claim_over_the_file_and_mark_it() {
        let core = claimed_base_url();
        let rows = core.setting_rows();
        let row = rows
            .iter()
            .find(|r| r.path == "base_url")
            .expect("the claimed path is a row");
        assert_eq!(row.value, "http://127.0.0.1:7897");
        assert!(row.changed, "the file still says 7896");

        let mut core = core_with_file("model = \"flash\"");
        core.claimed
            .insert("margins".to_string(), toml::Value::Integer(2));
        let rows = core.setting_rows();
        let added = rows.iter().find(|r| r.path == "margins").expect("added");
        assert_eq!(added.value, "2");
        assert!(added.changed);
    }

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            dir: std::path::PathBuf::from("/nowhere").join(name),
        }
    }

    #[test]
    fn a_skill_cannot_take_a_built_in_word() {
        // A repository contributes skills, and one that could redefine /new
        // would be a checkout taking the session over.
        let found = [skill("new", "not this one"), skill("archify", "diagrams")];
        let mut notes = Vec::new();
        let table = commands(&found, &mut notes);
        let new = table.iter().find(|c| c.word == "/new").unwrap();
        assert!(matches!(new.source, Source::Builtin));
        assert_eq!(table.iter().filter(|c| c.word == "/new").count(), 1);
        assert!(table.iter().any(|c| c.word == "/archify"));
        // Silently absent is how a user goes looking in the wrong place.
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("/new"), "{}", notes[0]);
    }

    // The gate, table by table. Every input reaches `fate`, so these stand in
    // for the key presses and phone messages that used to be answered by hand
    // in the loop — where nothing could test them without a terminal.

    #[test]
    fn a_key_and_a_line_that_mean_the_same_thing_are_one_intent() {
        // `ctrl+l` twice returns `Intent::New` directly. This is the other
        // half: the typed word lands on the same variant, so there is no
        // second value for `fate` to answer differently about.
        assert_eq!(read("/new"), Intent::New);
    }

    #[test]
    fn a_submitted_line_inherits_the_fate_of_what_it_says() {
        for line in [
            "/new",
            "/status",
            "/compact keep this",
            "fix the bug",
            "!ls",
            "/nope",
        ] {
            assert_eq!(
                format!("{:?}", Intent::Submit(line.into()).fate()),
                format!("{:?}", read(line).fate()),
                "{line}"
            );
        }
    }

    #[test]
    fn leaving_is_never_refused() {
        // One intent for `/exit`, `/quit`, ctrl+d and a double ctrl+c — the
        // words are checked with the rest of the table — and it always
        // proceeds: a wedged run must not be able to trap the user.
        assert!(matches!(Intent::Quit.fate(), Fate::Now));
    }

    #[test]
    fn reaching_for_the_transcript_is_refused_while_a_run_holds_it() {
        for intent in [
            Intent::New,
            Intent::Resume("1756240000-1".into()),
            Intent::Compact(String::new()),
            Intent::OpenRewind,
        ] {
            assert!(
                matches!(intent.fate(), Fate::Refused(_)),
                "{intent:?} should be refused"
            );
        }
    }

    #[test]
    fn what_the_run_is_not_standing_on_goes_through() {
        for intent in [
            Intent::Status,
            Intent::Help,
            Intent::Keys,
            Intent::Reload,
            Intent::Model(String::new()),
            Intent::Worktree("tree".into()),
            Intent::Interrupt,
            Intent::Unsend,
            Intent::SettingEdit("a.b".into(), "1".into()),
            Intent::SettingWrite("a.b".into()),
            Intent::SettingRevert("a.b".into()),
            Intent::Wechat("on".into()),
        ] {
            assert!(
                matches!(intent.fate(), Fate::Now),
                "{intent:?} should proceed"
            );
        }
    }

    #[test]
    fn what_wants_the_model_or_the_surface_waits() {
        for intent in [
            Intent::Bash("ls".into()),
            Intent::Settings(String::new()),
            Intent::Other {
                word: "/commit".into(),
                args: String::new(),
            },
        ] {
            assert!(
                matches!(intent.fate(), Fate::Queued),
                "{intent:?} should wait"
            );
        }
    }

    // Prose is the one thing a working run can still hear, so it does not
    // wait for one — waiting is what makes a correction arrive too late.
    #[test]
    fn prose_reaches_the_run_rather_than_waiting_for_it() {
        let said = "the bug is in parse.rs";
        assert!(matches!(Intent::Prompt(said.into()).fate(), Fate::Steered(text) if text == said));
        // A raw line answers as whatever it reads as, so both doors agree.
        assert!(matches!(
            Intent::Submit(said.into()).fate(),
            Fate::Steered(text) if text == said
        ));
        // A slash word is not prose and still waits.
        assert!(matches!(
            Intent::Submit("/settings".into()).fate(),
            Fate::Queued
        ));
    }

    // The transcript and the screen want different strings from a `!` line:
    // the model needs the command *and* its output, the reader needs the line
    // they typed. Storing only the first is what made the rebuilt scrollback
    // print `Ran \`git status\`` as a prompt with the output indented beneath.
    #[test]
    fn a_bang_line_stores_what_was_typed_beside_what_was_sent() {
        let mut s = Session::new();
        s.push_bash(Prompt {
            text: "Ran `git status`\nnothing to commit".into(),
            image: None,
            shown: Some("!git status".into()),
        });

        let Entry::Bash { run: t, .. } = &s.entries()[0] else {
            panic!("a text entry")
        };
        assert!(
            t.text.contains("nothing to commit"),
            "the model reads the output"
        );
        assert_eq!(
            t.shown.as_deref(),
            Some("!git status"),
            "the reader sees the line"
        );
    }

    // One lane that has spent nothing, enough for `/settings` to answer.
    fn a_lane(name: &str) -> crate::lane::Lane {
        let dir = std::env::temp_dir();
        let ws = tools::Workspace::new(&dir).expect("a workspace");
        let (events, inbox) = crate::lane::Lane::channel();
        crate::lane::Lane {
            token: crate::lane::next_token(),
            agent: std::sync::Arc::new(agent::Agent::new(
                std::sync::Arc::new(Recording::default()),
                test_spec("m"),
            )),
            session: None,
            id: name.into(),
            created: 0,
            name: Some(name.to_string()),
            totals: agent::Totals::default(),
            context: Vec::new(),
            standing: std::sync::Arc::from(""),
            ctx: tools::Ctx::new(ws),
            worktree: None,
            events,
            inbox,
            pending: Vec::new(),
            looping: None,
            pending_round: None,
            turn: crate::lane::Turn::Idle,
            view: Default::default(),
            keys: std::sync::Arc::new(crate::keys::Keys::default()),
            commands: std::sync::Arc::new(Vec::new()),
        }
    }

    // A transport that records the model each request asks for, and answers
    // one empty turn.
    #[derive(Default)]
    struct Recording {
        saw: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl brain::Transport for Recording {
        async fn stream(
            &self,
            spec: &brain::model::ModelSpec,
            _req: &brain::request::Request,
        ) -> brain::Result<
            futures::stream::BoxStream<'static, brain::Result<brain::stream::StreamEvent>>,
        > {
            self.saw.lock().unwrap().push(spec.model.clone());
            let done = Ok(brain::stream::StreamEvent::Done {
                stop: brain::stream::StopReason::EndTurn,
                usage: brain::stream::Usage::default(),
            });
            Ok(Box::pin(futures::stream::iter(std::iter::once(done))))
        }
    }

    fn test_spec(model: &str) -> brain::model::ModelSpec {
        brain::model::ModelSpec {
            model: model.into(),
            base_url: "http://localhost".into(),
            format: brain::model::Format::Anthropic {
                cache_control: brain::model::CacheControl::Off,
            },
            context_window: 200_000,
            max_output_tokens: 8_000,
            vision: true,
            thinking: Some(brain::model::ThinkingControl::Budget),
            accepts_temperature: true,
            can_force_tool: true,
            replay_thinking: brain::model::ReplayThinking::Tagged,
            pricing: brain::model::Pricing {
                input_per_mtok: 1.0,
                output_per_mtok: 2.0,
                ..Default::default()
            },
        }
    }

    // One lane on the given transport, with a subagent hung off it.
    fn a_repl(
        root: &std::path::Path,
        transport: std::sync::Arc<Recording>,
        model: &str,
    ) -> crate::repl::Repl {
        let ws = tools::Workspace::new(root).unwrap();
        let mut agent = agent::Agent::new(transport, test_spec(model));
        let store = crate::session::Store::new(root.join("state"));
        agent.hang(
            crate::subagent::Filed::armed(
                crate::session::Store::new(root.join("state")),
                root.to_path_buf(),
                model.into(),
            ),
            "standing",
        );

        let keys = std::sync::Arc::new(crate::keys::Keys::default());
        let commands = std::sync::Arc::new(Vec::<crate::repl::Command>::new());
        let (events, inbox) = crate::lane::Lane::channel();
        let lane = crate::lane::Lane {
            token: crate::lane::next_token(),
            agent: std::sync::Arc::new(agent),
            session: Some(agent::session::Session::default()),
            id: "s1".into(),
            created: 0,
            name: None,
            context: Vec::new(),
            standing: std::sync::Arc::from("standing"),
            totals: agent::Totals::default(),
            ctx: tools::Ctx::new(ws),
            worktree: None,
            events,
            inbox,
            pending: Vec::new(),
            looping: None,
            pending_round: None,
            turn: crate::lane::Turn::Idle,
            view: Default::default(),
            keys: keys.clone(),
            commands: commands.clone(),
        };
        crate::repl::Repl {
            store,
            keys,
            config: std::sync::Arc::new(crate::config::Config::default()),
            args: std::sync::Arc::new(<crate::Args as clap::Parser>::parse_from(["pi"])),
            commands,
            file: toml::Value::Table(Default::default()),
            claimed: Default::default(),
            lanes: vec![lane],
            current: 0,
        }
    }

    // What the child asked for, once.
    async fn run_the_child(core: &crate::repl::Repl) {
        let task = core
            .lane()
            .agent
            .registry
            .get(agent::task::Task::NAME)
            .unwrap();
        let ctx = tools::Ctx::new(core.lane().ctx.workspace.clone());
        task.execute(
            serde_json::json!({ "description": "go", "prompt": "go" }),
            &ctx,
        )
        .await
        .unwrap();
    }

    // `/model` retargets the lane's agent and rebuilds the subagent behind
    // it; the rebuild has to carry the new endpoint and model, or a child
    // called after the switch keeps talking to the old one with the old key.
    #[tokio::test]
    async fn retarget_rebuilds_the_subagent_on_the_new_model() {
        let dir = tempfile::tempdir().unwrap();
        let old_saw = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let old_transport = std::sync::Arc::new(Recording {
            saw: old_saw.clone(),
        });
        let mut core = a_repl(dir.path(), old_transport, "model-a");

        let new_saw = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let new_transport = std::sync::Arc::new(Recording {
            saw: new_saw.clone(),
        });
        core.retarget(new_transport, test_spec("model-b"));
        run_the_child(&core).await;

        let saw = new_saw.lock().unwrap();
        assert!(
            saw.iter().any(|m| m == "model-b"),
            "the child asks the new model: {saw:?}"
        );
        assert!(!saw.iter().any(|m| m == "model-a"), "{saw:?}");
        assert!(
            old_saw.lock().unwrap().is_empty(),
            "the old endpoint is gone"
        );
    }

    #[test]
    fn remove_worktree_closes_idle_lane_in_same_run() {
        let dir = crate::worktree::test_repo();
        let transport = std::sync::Arc::new(Recording::default());
        let mut core = a_repl(dir.path(), transport, "model-a");

        core.enter_worktree("fix-tools").unwrap();
        assert_eq!(core.lanes.len(), 2);
        assert_eq!(core.current, 1);

        core.current = 0;
        core.in_force();

        let res = core.remove_worktree("fix-tools");
        assert!(res.is_ok(), "remove_worktree failed: {res:?}");
        assert_eq!(core.lanes.len(), 1);
        assert_eq!(core.current, 0);
        assert!(!dir.path().join(".worktrees/fix-tools").exists());
    }

    #[test]
    fn remove_worktree_updates_current_index_when_earlier_lane_is_closed() {
        let dir = crate::worktree::test_repo();
        let transport = std::sync::Arc::new(Recording::default());
        let mut core = a_repl(dir.path(), transport, "model-a");

        core.enter_worktree("feat-one").unwrap();
        core.enter_worktree("feat-two").unwrap();
        assert_eq!(core.lanes.len(), 3);
        assert_eq!(core.current, 2);

        let res = core.remove_worktree("feat-one");
        assert!(res.is_ok(), "remove_worktree failed: {res:?}");
        assert_eq!(core.lanes.len(), 2);
        assert_eq!(core.current, 1);
        assert_eq!(core.lane().worktree.as_deref(), Some("feat-two"));
    }

    #[test]
    fn remove_worktree_refuses_when_another_lane_is_running() {
        let dir = crate::worktree::test_repo();
        let transport = std::sync::Arc::new(Recording::default());
        let mut core = a_repl(dir.path(), transport, "model-a");

        core.enter_worktree("fix-tools").unwrap();
        assert_eq!(core.lanes.len(), 2);

        core.lanes[1].turn = crate::lane::Turn::Running {
            cancel: tokio_util::sync::CancellationToken::new(),
            steer: None,
            unsend: false,
        };

        core.current = 0;
        core.in_force();

        let err = core.remove_worktree("fix-tools").unwrap_err();
        assert!(err.contains("running in another lane"), "{err}");
        assert_eq!(core.lanes.len(), 2);
        assert!(dir.path().join(".worktrees/fix-tools").exists());
    }
}
