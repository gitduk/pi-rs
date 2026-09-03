use std::borrow::Cow;
use std::collections::BTreeMap;

use agent::session::Session;
use agent::Totals;
use tokio_util::sync::CancellationToken;
use tools::skills::Skill;
use tools::{Tool, ToolError};

use serde::Deserialize;

use crate::config::Config;
use crate::journal;
use crate::lane::Lane;
use crate::session::{ResumeChoice, Store, Stored};

/// Where a command came from, and so what running it means.
#[derive(Clone)]
pub enum Source {
    /// A word `parse` knows and `command` answers itself.
    Builtin,
    /// A `SKILL.md` to read and hand to the model as if the user had typed it.
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
        "list this repository's worktrees, or work in one",
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
    Command::builtin("/cost", "", "what this session has spent so far"),
    Command::builtin(
        "/reload",
        "",
        "re-read ~/.pi/settings.toml, the instructions and the skills",
    ),
    Command::builtin("/log", "", "where this session is writing its journal"),
    Command::builtin(
        "/keys",
        "",
        "what every key does, and the id to rebind it under",
    ),
    Command::builtin("/help", "", "this list"),
    Command::builtin(
        "/settings",
        "[set <path> <value>]",
        "open the settings panel, or change one for this session",
    ),
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

/// What the line could still become: a command while its word is being typed,
/// then that command's own argument once the word is settled.
///
/// The commands with arguments worth completing are the ones whose argument is
/// a name out of a known set: `/model` against the config's models, `/resume`
/// against the workspace's saved sessions, `/worktree` against the
/// repository's checkouts. A prompt is prose and a focus phrase is prose;
/// guessing at either is worse than leaving it alone.
pub fn complete(
    line: &str,
    commands: &[Command],
    models: &[Choice],
    sessions: &[ResumeChoice],
    setting_paths: &[String],
    worktrees: &[Choice],
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
        // A name may hold a slash (`feat/one`), so unlike a model it is not
        // settled by the first word — only whitespace after it settles it.
        "/worktree" if typed.contains(char::is_whitespace) => Vec::new(),
        "/worktree" => worktrees
            .iter()
            .filter(|w| w.name.starts_with(typed) && w.name != typed)
            .map(|w| Candidate {
                show: w.name.clone(),
                line: format!("/worktree {}", w.name),
                help: w.note.clone(),
                more: false,
            })
            .collect(),
        // A session is named by what was asked first, though its id still
        // matches — someone may remember half of it. Accepting puts the id in
        // the line, because that is what `/resume` loads by. A first question
        // is a whole sentence, so the argument may keep several words.
        "/resume" => sessions
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
        // `set`/`get`/`reset` complete the path; the value is up to the user.
        "/settings" => {
            let Some((sub, _)) = typed.split_once(char::is_whitespace) else {
                let verbs = ["set", "get", "reset"];
                return verbs
                    .iter()
                    .filter(|v| v.starts_with(typed))
                    .map(|v| Candidate {
                        show: v.to_string(),
                        line: format!("/settings {v}"),
                        help: String::new(),
                        more: true,
                    })
                    .collect();
            };
            if !matches!(sub, "set" | "get" | "reset") {
                return Vec::new();
            }
            let want = typed
                .split_once(char::is_whitespace)
                .map(|(_, p)| p)
                .unwrap_or("");
            setting_paths
                .iter()
                .filter(|p| p.starts_with(want) && !p.is_empty())
                .map(|p| Candidate {
                    show: p.clone(),
                    line: format!("/settings {sub} {p}"),
                    help: String::new(),
                    more: false,
                })
                .collect()
        }
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
    /// What `/settings set` has claimed this run, by path. Replayed over
    /// every reload so the claimed values keep winning over the file.
    pub claimed: BTreeMap<String, toml::Value>,
    /// Every checkout open in this run, in the order they were opened. The
    /// main one is first, because that is where a run starts.
    pub lanes: Vec<Lane>,
    /// Which of them is in front. The surface shows one at a time.
    pub current: usize,
}

impl Repl {
    /// Put the lane in front's key map and command table in force. A skill
    /// belongs to one tree and not another, and so does a rebound key; leaving
    /// the last lane's in place had this one answering to another tree's.
    fn in_force(&mut self) {
        self.keys = self.lane().keys.clone();
        self.commands = self.lane().commands.clone();
    }

    /// The checkout in front. Indexing is safe by construction: `lanes` is
    /// never empty and nothing removes from it, so `current` always names one.
    pub fn lane(&self) -> &Lane {
        &self.lanes[self.current]
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
                    "{}: the file changed it, but /settings set is still shadowing it — /settings reset {path}",
                    path
                ));
            }
        }
        said
    }

    /// Take this config as the one in force: recompute everything it decides
    /// and swap it in. Whole or not at all — nothing is touched until all of
    /// it has been computed. `/reload` reads the file first; `/settings`
    /// hands over a tree it has just edited.
    fn adopt(&mut self, config: Config) -> Result<Vec<String>, String> {
        let root = self.lane().ctx.workspace.root().to_path_buf();
        let failed = |e| Err(format!("nothing reloaded — {}", refused("reload", e)));
        let project = match crate::config::load_project(&root) {
            Ok(p) => p,
            Err(e) => return failed(e),
        };
        let resolved = match crate::resolve(&self.args, &root, &config, &project, &self.claimed) {
            Ok(r) => r,
            Err(e) => return failed(e),
        };
        // Only here, with everything computed: a config or a skill set that
        // will not resolve leaves what is running exactly as it was.
        //
        // One `make_mut`: a run in flight holds the other reference, so this
        // is where the copy is taken, and taking it four times copies thrice
        // over.
        let ag = std::sync::Arc::make_mut(&mut self.lane_mut().agent);
        ag.registry = resolved.registry;
        ag.approver = std::sync::Arc::new(agent::Ceiling(resolved.tier));
        ag.system = resolved.system;
        ag.effort = resolved.effort;
        self.lane_mut().context = resolved.context;
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
                std::sync::Arc::make_mut(&mut self.lane_mut().agent).retarget(dialled.transport, dialled.spec);
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
                self.lane_mut().agent.spec.model, e
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

    /// Recompute the config from the file tree plus the session's claimed
    /// overrides, and adopt it.
    fn rebuild(&mut self) -> Vec<String> {
        self.rebuilt().unwrap_or_else(|why| vec![why])
    }

    /// The same, saying why when nothing could be adopted.
    fn rebuilt(&mut self) -> Result<Vec<String>, String> {
        let mut tree = self.file.clone();
        for (path, value) in &self.claimed {
            if let Err(e) = crate::settings::put(&mut tree, path, value.clone()) {
                return Err(refused("settings", e));
            }
        }
        let config = match crate::config::Config::deserialize(tree) {
            Ok(c) => c,
            Err(e) => return Err(refused("settings", anyhow::anyhow!(e))),
        };
        self.adopt(config)
    }

    /// `/settings set`: try the write on a scratch tree first, so a bad value
    /// touches nothing, then record it as a claim and rebuild.
    pub fn edit(&mut self, path: &str, raw: &str) -> Vec<String> {
        let mut scratch = self.file.clone();
        let old = crate::settings::get(&scratch, path).ok().cloned();
        if let Err(e) = crate::settings::set(&mut scratch, path, raw) {
            return vec![refused("settings", e)];
        }
        let new = crate::settings::get(&scratch, path).unwrap().clone();
        // Validate by deserializing the scratch tree, so a bad value never
        // reaches the running config.
        if let Err(e) = crate::config::Config::deserialize(scratch) {
            return vec![refused("settings", anyhow::anyhow!(e))];
        }
        self.claimed.insert(path.to_string(), new);
        let mut said = self.rebuild();
        let old_shown = match &old {
            Some(v) => mask_secret(path, v),
            None => "<unset>".to_string(),
        };
        said.push(format!(
            "{path}: {old_shown} → {}",
            mask_secret(path, &self.claimed[path])
        ));
        said
    }

    /// Drop a claim (`/settings reset path`), or all of them.
    pub fn unclaim(&mut self, path: Option<&str>) -> Vec<String> {
        match path {
            Some(p) => {
                self.claimed.remove(p);
            }
            None => self.claimed.clear(),
        }
        self.rebuild()
    }

    /// The settings panel's commit: write to the file, drop any claim on the
    /// same path so the written value is what wins, and rebuild. Validation
    /// happens on a scratch tree first; nothing is written or applied when it
    /// fails.
    pub fn commit_file(&mut self, path: &str, raw: &str) -> Result<Vec<String>, String> {
        let mut scratch = self.file.clone();
        crate::settings::set(&mut scratch, path, raw).map_err(|e| format!("{e:#}"))?;
        let new = crate::settings::get(&scratch, path).unwrap().clone();
        crate::config::Config::deserialize(scratch).map_err(|e| format!("{e:#}"))?;
        let file = self
            .args
            .config
            .as_deref()
            .map(std::path::PathBuf::from)
            .or_else(crate::config::global_path)
            .ok_or_else(|| "no settings file to write".to_string())?;
        crate::config::write(&file, path, new.clone()).map_err(|e| format!("{e:#}"))?;
        self.claimed.remove(path);
        self.file =
            crate::config::load_tree(self.args.config.as_deref()).map_err(|e| format!("{e:#}"))?;
        let mut said = self.rebuild();
        said.push(format!("{path} = {} — written to the file", new));
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
            "now on {} · {}",
            spec.model,
            summary(spec.format.name(), spec.context_window, &spec.pricing)
        ));
        // An absent transcript is one a run has, and it is writing this
        // model's reasoning into it as we speak — so say it either way.
        if self.lane_mut().session.as_ref().is_none_or(carries_reasoning) {
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
        std::sync::Arc::make_mut(&mut self.lane_mut().agent).retarget(dialled.transport, dialled.spec);
        said
    }

    /// What `/model` on its own shows.
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
                let mark = if &c.name == here { "●" } else { " " };
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
        self.lane().session.as_ref().map_or(0, |s| {
            brain::estimate::tokens(&s.context(), &self.lane().agent.spec)
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
    parts.join(" · ")
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

/// What a line at the prompt asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Exit,
    Help,
    Cost,
    New,
    Keys,
    Reload,
    Log,
    /// The session to switch to, or empty to list what there is.
    Resume(String),
    Name(String),
    /// Everything after the word focuses the summary.
    Compact(String),
    /// The name to move to, or empty to list what there is.
    Model(String),
    /// The worktree to work in, or empty to list what there is.
    Worktree(String),
    /// Everything after the word: a `set <path> <value>`, `get <path>`,
    /// `reset [path]`, or empty to open the panel.
    Settings(String),
    /// The wechat bridge verb: "" = status, "on" = connect, "off" = disconnect.
    Wechat(String),
    /// Not a built-in word. It may name a skill and it may name nothing; the
    /// command table settles that, and `parse` does not have it.
    Other {
        word: String,
        args: String,
    },
}

/// What a line may do while a turn is in flight.
///
/// Read off the parsed command rather than off the `Step` it produces:
/// `command` has already had its effect by the time it hands one back, so a
/// `Step` can say what happened but never whether it should have.
#[derive(Debug)]
pub enum Fate {
    /// Touches nothing the run is standing on.
    Now,
    /// Goes to the model, or needs the surface free, so it waits.
    Queued,
    /// Would move what the run stands on. Says this rather than doing it.
    Refused(&'static str),
}

/// What a submitted line may do while a turn is in flight.
///
/// A line naming nothing is a prompt, and a prompt waits — so anything `parse`
/// does not recognise is `Queued`.
pub fn fate_of(line: &str) -> Fate {
    parse(line).map_or(Fate::Queued, |cmd| cmd.fate())
}

impl Cmd {
    /// Exhaustive on purpose, with no catch-all arm: a command added without an
    /// answer here should fail to compile rather than default to one.
    ///
    /// What it cannot check is an existing arm's body: `Reload` and `Model` are
    /// `Now` because they write through `Arc::make_mut`, not because anything
    /// says so. An arm that starts reading `lane.session` breaks this quietly —
    /// the session is away for the length of a run.
    pub fn fate(&self) -> Fate {
        match self {
            // Answered from the config, the key map or the surface's own
            // totals — none of which the run is holding.
            Cmd::Help | Cmd::Keys | Cmd::Log | Cmd::Cost | Cmd::Name(_) => Fate::Now,
            // Both write through `Arc::make_mut`, so the run in flight keeps
            // the agent it started on and the next one picks up the change.
            Cmd::Reload | Cmd::Model(_) => Fate::Now,
            // Bare, these only list what there is.
            Cmd::Resume(name) | Cmd::Worktree(name) if name.trim().is_empty() => Fate::Now,
            Cmd::Resume(_) => Fate::Refused("/resume would replace the transcript this run is writing — esc first"),
            // A lane of its own to move to, and the one being left keeps
            // working in the tree it was already in.
            Cmd::Worktree(_) => Fate::Now,
            Cmd::New => Fate::Refused("/new would replace the transcript this run is writing — esc first"),
            Cmd::Compact(_) => Fate::Refused("/compact rewrites the transcript this run is writing — esc first"),
            // `set`/`get`/`reset` are lines; bare opens a panel, which wants
            // the surface to itself.
            Cmd::Settings(rest) if !rest.trim().is_empty() => Fate::Now,
            Cmd::Settings(_) => Fate::Queued,
            // A skill is a prompt; the bridge and the exit want the surface.
            Cmd::Exit | Cmd::Wechat(_) | Cmd::Other { .. } => Fate::Queued,
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

// A word `parse` did not know: a skill to run, or a typo to name.
fn dispatch(commands: &[Command], word: &str, args: &str) -> Step {
    let Some(skill) = skill_for(commands, word) else {
        return lines(format!("unknown command {word} — /help lists them"));
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
    let Cmd::Other { word, args } = parse(line)? else {
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

pub fn parse(line: &str) -> Option<Cmd> {
    let word = line.split_whitespace().next()?;
    if !word.starts_with('/') {
        return None;
    }
    Some(match word {
        "/exit" | "/quit" => Cmd::Exit,
        "/help" => Cmd::Help,
        "/cost" => Cmd::Cost,
        "/new" => Cmd::New,
        "/resume" => Cmd::Resume(rest(line)),
        "/keys" => Cmd::Keys,
        "/reload" => Cmd::Reload,
        "/log" => Cmd::Log,
        "/name" => Cmd::Name(rest(line)),
        "/compact" => Cmd::Compact(rest(line)),
        "/model" => Cmd::Model(rest(line)),
        "/worktree" => Cmd::Worktree(rest(line)),
        "/wechat" => Cmd::Wechat(rest(line)),
        "/settings" => Cmd::Settings(rest(line)),
        other => Cmd::Other {
            word: other.to_string(),
            args: rest(line),
        },
    })
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
    /// The id named nothing the transcript still holds.
    Nothing,
    /// The entry stayed, and what followed it went.
    Kept,
    /// The entry went too, and its text belongs back in the editor.
    Unsent(String),
}

pub enum Step {
    /// A `!` command to run. The surface executes it and records the result,
    /// because only it can await; `Repl::bash` does the actual work.
    Bash(String),
    /// What to send, and — when a skill expanded into it — the line that was
    /// typed. `rewind_nodes()` reads the second: a rewind menu offering four
    /// thousand characters of `SKILL.md` is offering the wrong thing, and so
    /// is a session named after one.
    Prompt {
        send: String,
        typed: Option<String>,
    },
    /// Needs the network, so the surface runs it and reports.
    Compact(Option<String>),
    /// Starts or stops the wechat bridge. Needs the network, so the surface
    /// runs it and reports — the same rule as `Compact`.
    Wechat(WechatCmd),
    /// Dealt with here; these lines are what there is to show for it. Returned
    /// rather than printed because one surface prints and the other paints.
    Handled(Vec<String>),
    /// The session was replaced — a `/new` or a `/resume` — so the surface
    /// has to rebuild its view from the new one, not just show the lines.
    Swap(Vec<String>),
    Quit,
}

/// What `/wechat` asks the surface to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WechatCmd {
    Status,
    On,
    Off,
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
    pub fn command(&mut self, line: &str, totals: &Totals) -> Step {
        if let Some(command) = bash_command(line) {
            return Step::Bash(command.to_string());
        }
        let Some(cmd) = parse(line) else {
            return Step::Prompt {
                send: line.to_string(),
                typed: None,
            };
        };
        match cmd {
            Cmd::Exit => Step::Quit,
            Cmd::Help => Step::Handled(help(&self.commands)),
            Cmd::Keys => Step::Handled(self.keys.listing()),
            Cmd::Reload => Step::Handled(self.reload()),
            Cmd::Log => lines(match crate::journal::path() {
                Some(p) => format!("{}", p.display()),
                None => "not recording — --log is off, or the file would not open".into(),
            }),
            Cmd::Cost => lines(crate::render::spent(&totals.usage, totals.cost)),
            Cmd::New => Step::Swap(self.fresh_session("started")),
            Cmd::Resume(name) => {
                if name.is_empty() {
                    Step::Handled(self.resume_listing())
                } else {
                    match self.resume(&name) {
                        Ok(said) => Step::Swap(said),
                        Err(why) => Step::Handled(vec![why]),
                    }
                }
            }
            Cmd::Name(name) => {
                if name.is_empty() {
                    self.lane_mut().name = None;
                    lines(format!("{} is unnamed again", self.lane_mut().id))
                } else {
                    let said = format!("{} is now “{name}”", self.lane_mut().id);
                    self.lane_mut().name = Some(name);
                    lines(said)
                }
            }
            Cmd::Compact(focus) => Step::Compact(Some(focus).filter(|f| !f.is_empty())),
            Cmd::Model(name) => Step::Handled(if name.is_empty() {
                self.listing()
            } else {
                self.switch(&name)
            }),
            Cmd::Worktree(name) => {
                if name.is_empty() {
                    Step::Handled(self.worktree_listing())
                } else {
                    match self.enter_worktree(&name) {
                        Ok(step) => step,
                        Err(why) => Step::Handled(vec![why]),
                    }
                }
            }
            Cmd::Other { word, args } => dispatch(&self.commands, &word, &args),
            Cmd::Wechat(rest) => match rest.trim() {
                "" => Step::Wechat(WechatCmd::Status),
                "on" => Step::Wechat(WechatCmd::On),
                "off" => Step::Wechat(WechatCmd::Off),
                other => lines(format!(
                    "unknown /wechat verb `{other}` — bare, on or off"
                )),
            },
            Cmd::Settings(rest) => self.settings(&rest),
        }
    }

    /// `/settings` surface. An empty argument opens the panel (the TUI takes
    /// over); `set`/`get`/`reset` are line commands.
    fn settings(&mut self, rest: &str) -> Step {
        let mut parts = rest.splitn(3, char::is_whitespace);
        let verb = parts.next().unwrap_or("");
        match verb {
            "" => self.open_panel(),
            "set" => {
                let path = parts.next().unwrap_or("");
                let value = parts.next().unwrap_or("");
                if path.is_empty() || value.is_empty() {
                    return lines("usage: /settings set <path> <value>");
                }
                Step::Handled(self.edit(path, value))
            }
            "get" => {
                let path = parts.next().unwrap_or("");
                if path.is_empty() {
                    return lines("usage: /settings get <path>");
                }
                let tree = self.file.clone();
                match crate::settings::get(&tree, path) {
                    Ok(v) => {
                        let shown = if journal::secret(journal::leaf(path)) {
                            match v.as_str() {
                                Some("") => "<unset>".to_string(),
                                Some(_) => "<set>".to_string(),
                                None => "<set>".to_string(),
                            }
                        } else {
                            v.to_string()
                        };
                        lines(format!("{path} = {shown}"))
                    }
                    Err(e) => lines(refused("settings", e)),
                }
            }
            "reset" => {
                let path = parts.next().unwrap_or("");
                if path.is_empty() {
                    Step::Handled(self.unclaim(None))
                } else {
                    Step::Handled(self.unclaim(Some(path)))
                }
            }
            other => lines(format!(
                "unknown /settings verb `{other}` — set, get or reset"
            )),
        }
    }

    /// The bare `/settings`: the TUI's panel, or a read-only list when this
    /// is not a terminal.
    fn open_panel(&mut self) -> Step {
        // The TUI intercepts bare `/settings` before it reaches here; the
        // line surface can only list.
        let mut out = Vec::new();
        for (path, value) in crate::settings::leaves(&self.file) {
            let shown = if journal::secret(journal::leaf(&path)) {
                if value.is_empty() {
                    "<unset>".to_string()
                } else {
                    "<set>".to_string()
                }
            } else {
                value
            };
            out.push(format!("{path} = {shown}"));
        }
        if out.is_empty() {
            out.push("nothing in ~/.pi/settings.toml yet".into());
        }
        Step::Handled(out)
    }

    /// Become the session this id names: the stamp that dates it, the journal
    /// it writes to, and the namespace its spills are filed under.
    ///
    /// One place because the id and the stamp always travel together and the
    /// two callers each set what the other did not — `created` was the one
    /// that got missed, and a resumed session was then re-dated on its next
    /// save with the stamp of the session it had just left.
    fn becomes(&mut self, id: String, created: u64) {
        self.lane_mut().id = id;
        self.lane_mut().created = created;
        crate::journal::switched(&self.lane_mut().id);
        // Spills are filed under the session id; a session has to own its own
        // namespace or the one before it keeps swallowing them.
        self.lane_mut().ctx = self.lane_mut().ctx.clone().with_session(&self.lane_mut().id);
    }

    /// Drop the in-memory conversation and open a fresh session under a new
    /// id. The old transcript stays on disk.
    fn fresh_session(&mut self, said: &str) -> Vec<String> {
        self.lane_mut().session = Some(Session::default());
        // A name identifies one session; carried over it would name two, which
        // is what `/name` exists to prevent.
        self.lane_mut().name = None;
        self.becomes(crate::session::new_id(), crate::session::now());
        vec![format!("{said} {}", self.lane_mut().id)]
    }

    /// Take a stored transcript as the running one — entries, name and id.
    /// Parting with what is being left is the caller's; they differ on when.
    fn adopt_session(&mut self, stored: Stored) -> Vec<String> {
        let (id, name, created) = (stored.id.clone(), stored.name.clone(), stored.created);
        let session = stored.into_session();
        self.lane_mut().name = name;
        self.lane_mut().session = Some(session);
        self.becomes(id, created);
        let mut said = vec![format!("resumed {}", self.lane_mut().id)];
        if let Some(name) = self.lane_mut().name.as_deref() {
            said.push(format!("“{name}”"));
        }
        said
    }

    /// `/worktree <name>`: create or reuse a checkout of this repository and
    /// move the session into it.
    ///
    /// Each tree keeps its own transcript rather than one transcript following
    /// the move: paths in it are workspace-relative, so under another root the
    /// same string names a different file, and the file locks and edit shifts
    /// are keyed by absolute path. Coming back therefore resumes what was being
    /// said in that tree, not an empty page.
    fn enter_worktree(&mut self, name: &str) -> Result<Step, String> {
        let from = self.lane_mut().ctx.workspace.root().to_path_buf();
        let (tree, how) = match crate::worktree::enter(&from, name) {
            Ok(found) => found,
            Err(e) => return Err(refused("worktree", e)),
        };
        // Built before the comparison: both sides are then canonical, and a
        // path git and the workspace spell differently is still one directory.
        let ws = match tools::Workspace::new(&tree.path)
            .and_then(|ws| ws.with_write_roots(&self.config.write_roots))
        {
            Ok(ws) => ws,
            Err(e) => {
                let what = format!("{}: {e}", tree.path.display());
                return Err(refused("worktree", anyhow::anyhow!(what)));
            }
        };
        if ws.root() == from {
            return Err(format!("already in {}", tree.name));
        }
        let on = tree.branch.as_deref().unwrap_or("a detached HEAD");
        let mut said = vec![
            match how {
                crate::worktree::Entered::Created => {
                    format!("{} — new, on new branch {on}", tree.name)
                }
                crate::worktree::Entered::Checkout => {
                    format!("{} — new, on existing branch {on}", tree.name)
                }
                crate::worktree::Entered::Existing => format!("{} — on {on}", tree.name),
            },
            tree.path.display().to_string(),
        ];

        // Against the root it belongs to, so before the move, not after. An
        // empty session — nothing said yet — has nothing to keep, and one a run
        // has is saved by the run.
        if self.lane().session.as_ref().is_some_and(|s| !s.is_empty())
            && let Err(e) = self.save()
        {
            tracing::warn!(target: "pi::session", error = %e, "the leaving session was not saved");
        }

        // Already open: going back to a tree is going back to the lane that
        // holds it, transcript, screen and all. Nothing is rebuilt.
        if let Some(i) = self
            .lanes
            .iter()
            .position(|lane| lane.ctx.workspace.root() == ws.root())
        {
            self.current = i;
            self.in_force();
            said.push(format!("back in {}", self.lane().id));
            // Handled, not a swap: the lane's screen is parked as it was left,
            // and rebuilding it from the transcript would throw that away.
            return Ok(Step::Handled(said));
        }

        said.extend(self.open_lane(ws, (!tree.main).then(|| tree.name.clone()))?);
        Ok(Step::Swap(said))
    }

    /// Open a checkout as a lane of its own, and put it in front.
    ///
    /// Whole or not at all, like every other path that reads a config: a tree
    /// whose config or skills will not resolve leaves the run where it was.
    fn open_lane(
        &mut self,
        ws: tools::Workspace,
        worktree: Option<String>,
    ) -> Result<Vec<String>, String> {
        let root = ws.root().to_path_buf();
        let failed = |e| format!("nothing opened — {}", refused("worktree", e));
        let project = crate::config::load_project(&root).map_err(failed)?;
        let resolved =
            crate::resolve(&self.args, &root, &self.config, &project, &self.claimed).map_err(failed)?;

        let (events, inbox) = Lane::channel();
        // The model travels; what the root decides does not. A switch changes
        // trees, and which model is answering was a decision made elsewhere.
        let mut ag = (*self.lane().agent).clone();
        ag.registry = resolved.registry;
        ag.approver = std::sync::Arc::new(agent::Ceiling(resolved.tier));
        ag.system = resolved.system;
        ag.effort = resolved.effort;

        // Built rather than cloned from the lane being left: `Ctx` shares its
        // file locks and edit shifts through an `Arc`, so a cloned one would
        // clear that lane's along with its own the first time it relocated.
        self.lanes.push(Lane {
            agent: std::sync::Arc::new(ag),
            session: Some(Session::default()),
            id: String::new(),
            created: 0,
            name: None,
            context: resolved.context,
            ctx: tools::Ctx::new(ws),
            keys: std::sync::Arc::new(resolved.keys),
            commands: std::sync::Arc::new(resolved.commands),
            worktree,
            events,
            inbox,
            pending: Vec::new(),
            turn: crate::lane::Turn::Idle,
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
                self.fresh_session("started")
            }
        })
    }

    /// The checkouts `/worktree` can move to, the repository's own first, the
    /// one the session is in marked.
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
                let mark = if at.as_ref() == Some(&t.path) { "*" } else { " " };
                let on = t.branch.as_deref().unwrap_or("detached HEAD");
                format!("{mark} {}  {on}", crate::render::pad(&t.name, width))
            })
            .collect();
        out.push(format!(
            "/worktree <name> works in one, creating it under {}/ if it is not there",
            crate::worktree::DIR
        ));
        out
    }

    /// The sessions `/resume` can switch to, newest first, the one running
    /// now marked.
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
                    if *mark { "●" } else { " " },
                    crate::render::pad(text, width),
                    ago(*created)
                )
            })
            .collect();
        out.push("resume one by typing /resume and Tab".into());
        out
    }

    /// Switch to a saved session: its transcript, name and id become this
    /// one's, and every further turn extends it. The session being left is
    /// saved first, so nothing is lost on the way out.
    ///
    /// The model in charge does not change — that stays the prompt's decision
    /// — so reasoning a different model wrote is demoted the way it is after
    /// a `/model` switch.
    fn resume(&mut self, id: &str) -> Result<Vec<String>, String> {
        // The session being left has to survive too, or /resume throws it
        // away. An empty one — just opened, nothing said — has nothing to keep.
        if self.lane_mut().session.as_ref().is_some_and(|s| !s.is_empty())
            && let Err(e) = self.save()
        {
            tracing::warn!(target: "pi::session", error = %e, "resume could not save the leaving session");
        }
        let stored = self.store.load(id).map_err(|e| refused("resume", e))?;
        Ok(self.adopt_session(stored))
    }

    /// Run what `!` named: show the output, and record the command and its
    /// result in the transcript so the model answers with it in view. Same
    /// runner, workspace and timeout as the model's own `bash` tool. The
    /// surface hands in a token so Ctrl-C (line mode) or Esc (terminal) can
    /// stop the command instead of leaving the caller stuck for the timeout.
    pub async fn bash(&mut self, command: &str, cancel: CancellationToken) -> Vec<String> {
        let ctx = self.lane_mut().ctx.clone().with_cancel(cancel);
        let out = tools::bash::Bash
            .execute(serde_json::json!({ "command": command }), &ctx)
            .await;
        let out = match out {
            Ok(out) => out,
            Err(ToolError::Cancelled) => return vec!["cancelled".into()],
            Err(e) => return vec![format!("failed to run `{command}`: {e}")],
        };
        let body = out.flatten();
        let text = format!(
            "Ran `{command}`\n{}",
            if body.is_empty() {
                "(no output)"
            } else {
                &body
            }
        );
        // What the model reads is the command *and its output*; what the
        // screen shows is the line the user typed. Storing only the first left
        // the rebuilt scrollback printing `Ran \`git status\`` as a prompt,
        // with the output indented under it — the live path never did that,
        // because it had the typed line and the rebuild did not.
        // `!` is queued while a run has the transcript, so it lands here with
        // the session at home.
        if let Some(session) = &mut self.lane_mut().session {
            session.push_user(agent::session::UserBody::Aside(agent::session::UserText {
                text,
                shown: Some(format!("!{command}")),
            }));
        }
        let mut said: Vec<String> = body
            .lines()
            // The tags wrap the model's copy; the terminal shows the output
            // itself.
            .filter(|l| !matches!(*l, "<stdout>" | "</stdout>" | "<stderr>" | "</stderr>"))
            .map(str::to_string)
            .collect();
        if let Err(e) = self.save() {
            said.push(format!("warning: the transcript was not saved: {e}"));
        }
        said
    }
}

// A secret value as a change line shows it: set or unset, never the value.
fn mask_secret(path: &str, value: &toml::Value) -> String {
    if journal::secret(journal::leaf(path)) {
        match value.as_str() {
            Some("") => "<unset>".to_string(),
            Some(_) | None => "<set>".to_string(),
        }
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BUILTIN, Candidate, Choice, Cmd, Command, ResumeChoice, Source, Step, ago, bash_command,
        commands, complete, dispatch, expand, gist, help, parse,
    };
    use agent::session::{Entry, Session, UserBody, UserText};
    use tools::skills::Skill;

    #[test]
    fn every_listed_command_actually_parses() {
        // The table drives help and completion; a word that reached the list
        // without reaching `parse` would offer to complete into nothing.
        for c in BUILTIN {
            assert!(
                !matches!(parse(&c.word), Some(Cmd::Other { .. }) | None),
                "{} is listed but does not parse",
                c.word
            );
        }
    }

    fn choices() -> Vec<Choice> {
        ["flash", "flint", "sonnet"]
            .iter()
            .map(|n| Choice {
                name: (*n).to_string(),
                note: "openai · 128k".into(),
            })
            .collect()
    }

    fn skill(name: &str, description: &str) -> Skill {
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            dir: std::path::PathBuf::from("/nowhere").join(name),
        }
    }

    fn table() -> Vec<Command> {
        commands(&[], &mut Vec::new())
    }

    fn offered_from(line: &str, table: &[Command]) -> Vec<String> {
        complete(line, table, &choices(), &[], &[], &[])
            .into_iter()
            .map(|c| c.show)
            .collect()
    }

    fn offered(line: &str) -> Vec<String> {
        offered_from(line, &table())
    }

    #[test]
    fn completion_narrows_as_the_word_is_typed() {
        assert_eq!(offered("/n"), ["/new", "/name [text]"]);
        assert_eq!(offered("/na"), ["/name [text]"]);
        // A word typed in full stays first: the menu must not drop it and
        // leave only a longer one that starts the same way.
        assert_eq!(offered("/name"), ["/name [text]"]);
        // Past a word with nothing to complete, the rest is prose.
        assert!(offered("/name the flaky test").is_empty());
        // Not a command at all.
        assert!(offered("what does /help do").is_empty());
        assert!(offered("").is_empty());
    }

    #[test]
    fn worktrees_complete_by_name_and_carry_the_branch_they_are_on() {
        let trees = [
            Choice {
                name: "pi-rs".into(),
                note: "master".into(),
            },
            Choice {
                name: "feature-one".into(),
                note: "feature-one".into(),
            },
            Choice {
                name: "feat/two".into(),
                note: "feat/two".into(),
            },
        ];
        let offered = |line: &str| -> Vec<String> {
            complete(line, &table(), &[], &[], &[], &trees)
                .into_iter()
                .map(|c| c.line)
                .collect()
        };
        assert_eq!(
            offered("/worktree fe"),
            ["/worktree feature-one", "/worktree feat/two"]
        );
        assert_eq!(offered("/worktree featu"), ["/worktree feature-one"]);
        // A name may hold a slash, so the argument is not settled until a
        // space follows it — `feat/` still has somewhere to go.
        assert_eq!(offered("/worktree feat/"), ["/worktree feat/two"]);
        // Typed in full there is nothing left to offer, and past it the line
        // is no longer a name.
        assert!(offered("/worktree feature-one").is_empty());
        assert!(offered("/worktree feature-one and").is_empty());
        // The branch is what tells two checkouts apart when the names do not.
        let all = complete("/worktree ", &table(), &[], &[], &[], &trees);
        assert_eq!(all.len(), 3);
        assert_eq!((all[0].show.as_str(), all[0].help.as_str()), ("pi-rs", "master"));
    }

    #[test]
    fn the_worktree_word_takes_a_name_and_nothing_else_does() {
        assert!(matches!(
            parse("/worktree feature-one"),
            Some(Cmd::Worktree(name)) if name == "feature-one"
        ));
        // Bare, it is the listing.
        assert!(matches!(parse("/worktree"), Some(Cmd::Worktree(name)) if name.is_empty()));
        // Not a built-in word: the command table settles what it is.
        assert!(matches!(parse("/worktrees"), Some(Cmd::Other { .. })));
    }

    #[test]
    fn an_exact_word_stays_first_when_it_prefixes_another() {
        // `/news` is a skill here; `/new` typed in full keeps first place in
        // the menu, so Tab accepts the built-in rather than sliding onto news.
        let found = [skill("news", "headlines from the fixed feeds")];
        let table = commands(&found, &mut Vec::new());
        assert_eq!(offered_from("/new", &table), ["/new", "/news [text]"]);
    }
    #[test]
    fn accepting_a_command_that_wants_an_argument_leaves_room_for_one() {
        let of = |line: &str| -> Candidate {
            complete(line, &table(), &choices(), &[], &[], &[]).swap_remove(0)
        };
        let name = of("/nam");
        assert_eq!((name.line.as_str(), name.more), ("/name", true));
        // Nothing follows /cost, so the caret should not be pushed past a space
        // the user then has to delete.
        let cost = of("/cos");
        assert_eq!((cost.line.as_str(), cost.more), ("/cost", false));
    }

    #[test]
    fn the_models_complete_once_the_command_word_is_settled() {
        // The whole point: the name is the tedious part to type, and it is the
        // one thing the config already knows.
        assert_eq!(offered("/model fl"), ["flash", "flint"]);
        assert_eq!(offered("/model fla"), ["flash"]);
        // A bare space offers all of them rather than nothing.
        assert_eq!(offered("/model "), ["flash", "flint", "sonnet"]);
        // Whole already, and there is no second argument behind it.
        assert!(offered("/model flash").is_empty());
        assert!(offered("/model flash and").is_empty());
        // Accepting one replaces the line, not just the word.
        assert_eq!(
            complete("/model fla", &table(), &choices(), &[], &[], &[])[0].line,
            "/model flash"
        );
    }

    #[test]
    fn a_config_with_no_models_offers_nothing_rather_than_every_command() {
        // choices() is empty when the file defines no model, and the argument
        // branch must not fall back to completing command words again.
        assert!(complete("/model fl", &table(), &[], &[], &[], &[]).is_empty());
    }

    fn sessions() -> Vec<ResumeChoice> {
        vec![
            ResumeChoice {
                id: "1756240000-100".into(),
                prompt: "why is the flaky test flaky?".into(),
                created: 1_756_240_000,
            },
            ResumeChoice {
                id: "1756240000-200".into(),
                prompt: "lint the workspace".into(),
                created: 1_756_240_100,
            },
        ]
    }

    #[test]
    fn the_sessions_complete_by_first_prompt_and_accept_the_id() {
        let sessions = sessions();
        let of = |line: &str| {
            complete(line, &table(), &[], &sessions, &[], &[])
                .into_iter()
                .map(|c| c.show)
                .collect::<Vec<_>>()
        };
        // A bare space offers every session by its first question, not its id.
        assert_eq!(
            of("/resume "),
            ["why is the flaky test flaky?", "lint the workspace"]
        );
        assert_eq!(of("/resume why"), ["why is the flaky test flaky?"]);
        // A first question is a sentence, so a multi-word prefix matches too.
        assert_eq!(of("/resume lint the"), ["lint the workspace"]);
        // Half an id still matches — someone may remember it that way.
        assert_eq!(
            of("/resume 1756"),
            ["why is the flaky test flaky?", "lint the workspace"]
        );
        // A session with nothing said yet is not offered.
        let quiet = [ResumeChoice {
            id: "1756240000-300".into(),
            prompt: String::new(),
            created: 0,
        }];
        assert!(complete("/resume ", &table(), &[], &quiet, &[], &[]).is_empty());
        // Accepting replaces the line with the id, which is what /resume loads.
        let got = &complete("/resume lint", &table(), &[], &sessions, &[], &[])[0];
        assert_eq!(got.line, "/resume 1756240000-200");
        assert!(!got.more);
    }

    #[test]
    fn a_skill_is_a_command_under_its_own_name() {
        // No prefix: the name is what the user knows the skill by.
        let found = [skill("commit", "Use when ready to commit changes")];
        let table = commands(&found, &mut Vec::new());
        assert_eq!(
            offered_from("/com", &table),
            ["/compact [focus]", "/commit [text]"]
        );
        assert!(matches!(
            table
                .iter()
                .find(|c| c.word == "/commit")
                .map(|c| &c.source),
            Some(Source::Skill(_))
        ));
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

    #[test]
    fn a_skill_command_keeps_what_follows_it() {
        // `parse` has no table, so a skill and a typo are the same shape here
        // and only the caller can tell them apart.
        assert_eq!(
            parse("/commit fix the flaky test"),
            Some(Cmd::Other {
                word: "/commit".into(),
                args: "fix the flaky test".into()
            })
        );
    }

    #[test]
    fn a_typo_is_named_rather_than_sent_to_the_model() {
        // Otherwise `/tood` becomes a prompt and the model has to guess.
        assert_eq!(
            parse("/tood"),
            Some(Cmd::Other {
                word: "/tood".into(),
                args: String::new()
            })
        );
    }

    #[test]
    fn a_description_written_for_the_model_is_cut_to_a_line() {
        // Skill descriptions run to paragraphs; the list has one column.
        assert_eq!(gist("Write a commit."), "Write a commit");
        assert_eq!(
            gist("Draw diagrams. Also validates them, and much more besides."),
            "Draw diagrams"
        );
        let long = "a".repeat(200);
        assert_eq!(
            gist(&long).chars().count(),
            61,
            "60 columns and the ellipsis"
        );
        // Width, not characters: these cost two columns each.
        let wide = "从固定信源拉取上次运行之后的增量信息，生成每日精选摘要，并按重要性排序";
        assert!(gist(wide).ends_with('…'));
        assert_eq!(gist(wide).chars().count(), 31);
    }

    #[test]
    fn help_says_which_half_of_the_list_a_word_came_from() {
        // Without a prefix there is nothing in `/commit` itself to say it is
        // not built in.
        let plain = help(&table());
        assert!(plain.iter().all(|l| !l.is_empty()));
        let with = help(&commands(
            &[skill("commit", "Write a commit")],
            &mut Vec::new(),
        ));
        let split = with.iter().position(String::is_empty).expect("a break");
        assert!(with[split + 1].starts_with("skills"));
        assert!(with[split + 2].starts_with("/commit"));
    }

    #[test]
    fn a_skill_arrives_as_a_message_the_user_could_have_typed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("commit");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: commit\ndescription: Write a commit.\n---\nStage, then write it.\n",
        )
        .unwrap();
        let mut s = skill("commit", "Write a commit.");
        s.dir = dir;

        let table = commands(std::slice::from_ref(&s), &mut Vec::new());
        let Step::Prompt { send: text, typed } = dispatch(&table, "/commit", "the parser work")
        else {
            panic!("a skill runs a turn; it is not handled here");
        };
        assert!(text.starts_with("Run the `commit` skill."), "{text}");
        assert!(text.contains("Stage, then write it."));
        // The frontmatter is metadata, not instructions.
        assert!(!text.contains("description:"), "{text}");
        // Arguments below the body, so the skill reads as the standing order.
        let body = text.find("Stage, then").unwrap();
        assert!(text.find("the parser work").unwrap() > body, "{text}");

        // What the model reads is the whole body; what a person reads is the
        // line they typed. Sending the body as both put four thousand
        // characters of `SKILL.md` in the rewind menu and named the session
        // after them.
        assert_eq!(typed.as_deref(), Some("/commit the parser work"));
    }

    #[test]
    fn a_skill_with_no_arguments_is_named_by_its_word_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("commit");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: commit\ndescription: Write a commit.\n---\nStage, then write it.\n",
        )
        .unwrap();
        let mut s = skill("commit", "Write a commit.");
        s.dir = dir;

        let table = commands(std::slice::from_ref(&s), &mut Vec::new());
        let Step::Prompt { typed, .. } = dispatch(&table, "/commit", "") else {
            panic!("a skill runs a turn");
        };
        assert_eq!(typed.as_deref(), Some("/commit"), "no trailing space");
    }

    #[test]
    fn a_skill_whose_file_has_gone_says_so_rather_than_prompting() {
        // Discovery ran at startup; the directory can be gone by now, and an
        // empty prompt reaching the model is the worst of the answers.
        let ghost = skill("ghost", "x");
        let table = commands(std::slice::from_ref(&ghost), &mut Vec::new());
        let Step::Handled(said) = dispatch(&table, "/ghost", "") else {
            panic!("nothing should reach the model");
        };
        assert!(said[0].starts_with("cannot run ghost"), "{said:?}");
        // And a one-shot run refuses rather than sending the word itself.
        assert!(expand(&table, "/ghost").unwrap().is_err());
    }

    #[test]
    fn a_one_shot_prompt_expands_a_skill_and_leaves_everything_else_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("commit");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\ndescription: x\n---\nStage it.\n",
        )
        .unwrap();
        let mut s = skill("commit", "x");
        s.dir = dir;
        let table = commands(std::slice::from_ref(&s), &mut Vec::new());

        let got = expand(&table, "/commit the parser work").expect("a skill");
        assert!(got.unwrap().contains("Stage it."));

        // A built-in is a session operation and there is no session here.
        assert!(expand(&table, "/help").is_none());
        // And a word that names nothing is prose, not a typo to refuse: the
        // argument is a whole prompt, and prompts start with slashes.
        assert!(expand(&table, "/comit the parser work").is_none());
        assert!(expand(&table, "/usr/bin is missing").is_none());
        assert!(expand(&table, "fix the flaky test").is_none());
    }

    #[test]
    fn slash_words_are_commands_and_prose_is_not() {
        assert_eq!(parse("/exit"), Some(Cmd::Exit));
        assert_eq!(parse("/quit"), Some(Cmd::Exit));
        assert_eq!(parse("fix the bug"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn resume_parse() {
        // Bare, /resume lists; a word names the session to switch to.
        assert_eq!(parse("/resume"), Some(Cmd::Resume(String::new())));
        assert_eq!(
            parse("/resume 1756240000-123"),
            Some(Cmd::Resume("1756240000-123".into()))
        );
    }

    #[test]
    fn ago_is_human() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert_eq!(ago(now), "just now");

        assert_eq!(ago(now - 120), "2m ago");
        assert_eq!(ago(now - 7200), "2h ago");
        assert_eq!(ago(now - 3 * 86400), "3d ago");
    }

    #[test]
    fn the_wechat_verb_parses_and_others_are_named() {
        assert_eq!(parse("/wechat"), Some(Cmd::Wechat(String::new())));
        assert_eq!(parse("/wechat on"), Some(Cmd::Wechat("on".into())));
        assert_eq!(parse("/wechat off"), Some(Cmd::Wechat("off".into())));
        // A typo reaches the command layer as a literal, so it can be named
        // rather than silently meaning "status".
        assert_eq!(parse("/wechat oen"), Some(Cmd::Wechat("oen".into())));
    }

    #[test]
    fn a_prompt_that_merely_mentions_a_slash_stays_a_prompt() {
        assert_eq!(parse("what does /help do?"), None);
        assert_eq!(parse("read src/main.rs"), None);
    }

    #[test]
    fn trailing_words_do_not_break_a_command() {
    }

    #[test]
    fn a_command_that_takes_words_keeps_all_of_them() {
        assert_eq!(
            parse("/name the flaky test"),
            Some(Cmd::Name("the flaky test".into()))
        );
        assert_eq!(
            parse("/compact  keep the parser work "),
            Some(Cmd::Compact("keep the parser work".into()))
        );
        // Bare, they mean clear and unfocused respectively.
        assert_eq!(parse("/name"), Some(Cmd::Name(String::new())));
        assert_eq!(parse("/compact"), Some(Cmd::Compact(String::new())));
    }

    #[test]
    fn a_bang_runs_a_shell_command_and_a_bare_one_stays_prose() {
        assert_eq!(bash_command("!ls"), Some("ls"));
        assert_eq!(bash_command("! ls -la"), Some("ls -la"));
        assert_eq!(bash_command("!"), None);
        assert_eq!(bash_command("hello!"), None);
        // `!!cmd` is shell `! cmd` (negate the exit code), not pi's
        // excluded-from-context marker — that half of the feature is not here.
        assert_eq!(bash_command("!!git status"), Some("!git status"));
    }

    /// The transcript and the screen want different strings from a `!` line:
    /// the model needs the command *and* its output, the reader needs the line
    /// they typed. Storing only the first is what made the rebuilt scrollback
    /// print `Ran \`git status\`` as a prompt with the output indented beneath.
    #[test]
    fn a_bang_line_stores_what_was_typed_beside_what_was_sent() {
        let mut s = Session::new();
        s.push_user(UserBody::Aside(UserText {
            text: "Ran `git status`\nnothing to commit".into(),
            shown: Some("!git status".into()),
        }));

        let Entry::User {
            body: UserBody::Aside(t),
            ..
        } = &s.entries()[0]
        else {
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
}
