//! A model name turned into something that can be talked to: its spec and the
//! client for its wire. Startup, `/model` and a reload all come through here.

use std::sync::Arc;

use anyhow::{Context, Result};
use llm::model::{Format, ModelSpec};
use llm::transport::{Transport, anthropic::Anthropic, chat::ChatCompletions, openai::OpenAi};

use crate::args::Pinned;
use pi_store::config;

fn transport_for(spec: &ModelSpec, configured: Option<String>) -> Arc<dyn Transport> {
    match spec.format {
        Format::Anthropic { .. } => Arc::new(Anthropic::new(configured)),
        Format::OpenAi => Arc::new(OpenAi::new(configured)),
        Format::Chat => Arc::new(ChatCompletions::new(configured)),
    }
}

/// One model, ready to talk to: what to send, and the client to send it with.
pub struct Dialled {
    pub spec: ModelSpec,
    pub transport: Arc<dyn Transport>,
    /// The guess made for a model the config does not describe. Said at
    /// startup (not under `--quiet`) and at every `/model`, which reports
    /// either way; a reload keeps the model and so has nothing new to say.
    pub assumed: Option<String>,
    /// Said even under `--quiet`: an exposed key is a fact about the
    /// machine, not progress chatter that silence should suppress.
    pub warning: Option<String>,
}

// A missing provider or incomplete connection names who selected the model.
fn unknown(model: &str, named_by: config::ModelOrigin) -> String {
    format!(
        "`{model}`, named by {}, cannot be reached — see examples/pi.toml",
        said_by(named_by)
    )
}

// Who named the model, as the user would recognise it.
fn said_by(origin: config::ModelOrigin) -> &'static str {
    use config::ModelOrigin as O;
    match origin {
        O::Flag => "-m",
        O::Command => "/model",
        O::Resumed => "the resumed session",
        O::Project => "`model` in the project's .pi/settings.toml",
        O::Global => "`model` in ~/.pi/settings.toml",
        O::OnlyModel => "the only model in ~/.pi/settings.toml",
        O::Discovered => "the endpoint's model list",
    }
}

/// Resolve a model name; startup and `/model` share this so the two agree.
/// The config is the only source of truth.
pub fn dial(
    pinned: &Pinned,
    config: &config::Config,
    model: &str,
    named_by: config::ModelOrigin,
) -> Result<Dialled> {
    let mut spec = config
        .find(model)
        .with_context(|| unknown(model, named_by))?;
    if let Some(url) = &pinned.base_url {
        spec.base_url = config::expand_base_url(url);
    }
    if let Some(window) = pinned.context {
        spec.context_window = window;
    }

    // Every fact nobody stated is a guess. Naming them lets the user correct
    // the one that matters instead of debugging a 400 later.
    let assumed = assumed(pinned, config, &spec);
    let key = config.key_for(model);
    let warning = config
        .written_key(model)
        .filter(|k| !k.starts_with('$'))
        .and_then(|_| {
            pinned
                .config
                .clone()
                .map(std::path::PathBuf::from)
                .or_else(config::global_path)
        })
        .and_then(|path| config::warn_if_exposed(&path));
    let transport = transport_for(&spec, key);
    Ok(Dialled {
        spec,
        transport,
        assumed,
        warning,
    })
}

// Every fact nobody stated, as one sentence; `None` when the files and the
// flags say all of them. A start and a list that never came share the rule.
pub fn assumed(pinned: &Pinned, config: &config::Config, spec: &ModelSpec) -> Option<String> {
    let entry = config.entry(&spec.model);
    let mut guessed: Vec<String> = Vec::new();
    let window_guessed = entry.context_window.is_none() && pinned.context.is_none();
    if window_guessed {
        guessed.push(format!("a {}-token window", spec.context_window));
    }
    if entry.max_output_tokens.is_none() {
        guessed.push(format!("{} max output", spec.max_output_tokens));
    }
    if entry.pricing == llm::model::Pricing::default() {
        guessed.push("no pricing".into());
    }
    (!guessed.is_empty()).then(|| {
        let hint = match window_guessed {
            true => ". Set --context if the server's window differs.",
            false => ".",
        };
        format!("assuming {} for `{}`{hint}", joined(&guessed), spec.model)
    })
}

// The facts as one list: `a`, `b` and `c`.
fn joined(facts: &[String]) -> String {
    match facts.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// The summarizer's own connection, when the config names a model for it: a
/// second dial, because a summary is a model call like any other and a cheaper
/// model for it is the point of the setting.
pub fn summary_writer(
    pinned: &Pinned,
    config: &config::Config,
) -> Result<Option<(Arc<dyn Transport>, ModelSpec)>> {
    match &config.summarize_model {
        Some(name) => {
            let summarizer = dial(pinned, config, name, config::ModelOrigin::Global)
                .with_context(|| format!("summarize_model = \"{name}\""))?;
            Ok(Some((summarizer.transport, summarizer.spec)))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(body: &str) -> config::Config {
        config::Config::in_force(toml::from_str(body).expect("a tree")).expect("a config")
    }

    fn assumed(config: &config::Config, model: &str) -> Option<String> {
        dial(
            &Pinned::default(),
            config,
            model,
            config::ModelOrigin::Command,
        )
        .expect("a model")
        .assumed
    }

    // Discovery states an ID and whatever limits the endpoint listed; the rest
    // falls back to built-in numbers, which must not read as measured.
    #[test]
    fn the_facts_nobody_stated_are_named_as_guesses() {
        let bare = config("[provider.local]\nbase_url = \":1/v1\"\nformat = \"chat\"\n");
        assert_eq!(
            assumed(&bare, "local/m").as_deref(),
            Some(
                "assuming a 128000-token window, 8192 max output and no pricing for `local/m`. \
                 Set --context if the server's window differs."
            )
        );

        let window_stated = config(
            "[provider.local]\nbase_url = \":1/v1\"\nformat = \"chat\"\n\
             [models.\"local/m\"]\ncontext_window = 500_000\n",
        );
        assert_eq!(
            assumed(&window_stated, "local/m").as_deref(),
            Some("assuming 8192 max output and no pricing for `local/m`.")
        );

        let stated = config(
            "[provider.local]\nbase_url = \":1/v1\"\nformat = \"chat\"\n\
             [models.\"local/m\"]\ncontext_window = 500_000\nmax_output_tokens = 4096\n\
             pricing = { input_per_mtok = 1.0 }\n",
        );
        assert_eq!(assumed(&stated, "local/m"), None);
    }

    // A window the flag states is not a guess, whatever the files say.
    #[test]
    fn a_flagged_window_is_not_assumed() {
        let bare = config("[provider.local]\nbase_url = \":1/v1\"\nformat = \"chat\"\n");
        let pinned = Pinned {
            context: Some(12_345),
            ..Pinned::default()
        };
        let dialled = dial(&pinned, &bare, "local/m", config::ModelOrigin::Command).unwrap();
        assert_eq!(dialled.spec.context_window, 12_345);
        assert_eq!(
            dialled.assumed.as_deref(),
            Some("assuming 8192 max output and no pricing for `local/m`.")
        );
    }
}
