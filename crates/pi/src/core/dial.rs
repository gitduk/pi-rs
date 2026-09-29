//! A model name turned into something that can be talked to: its spec and the
//! client for its wire. Startup, `/model` and `/reload` all come through here.

use std::sync::Arc;

use anyhow::{Context, Result};
use llm::model::{Format, ModelSpec};
use llm::transport::{Transport, anthropic::Anthropic, chat::ChatCompletions, openai::OpenAi};

use crate::args::Pinned;
use crate::store::config;

// `configured` is the config's `api_key`; the environment variable is the
// fallback. The two OpenAI-family wires share `OPENAI_API_KEY`; Anthropic
// takes `ANTHROPIC_API_KEY`. A key is never required: whichever is set rides
// along, and an endpoint that needs one answers with its own refusal.
fn transport_for(spec: &ModelSpec, configured: Option<String>) -> Arc<dyn Transport> {
    let key = configured.or_else(|| {
        let var = match spec.format {
            Format::Chat | Format::OpenAi => "OPENAI_API_KEY",
            Format::Anthropic { .. } => "ANTHROPIC_API_KEY",
        };
        std::env::var(var).ok()
    });
    match spec.format {
        Format::Anthropic { .. } => Arc::new(Anthropic::new(key)),
        Format::OpenAi => Arc::new(OpenAi::new(key)),
        Format::Chat => Arc::new(ChatCompletions::new(key)),
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
    /// Said even under `--quiet`, which is why it is not one of the notes. An
    /// exposed key is a fact about the machine rather than progress chatter,
    /// and the run that asked for silence is the scripted one nobody is
    /// watching — exactly the one that would never hear it again.
    pub warning: Option<String>,
}

// Why this run wanted that model, for an endpoint that cannot serve it.
//
// Every name resolves now — an unlisted one is passed through with default
// numbers — so the only way this fails is a config with no endpoint to send
// it to, and the useful half of that message is who asked.
fn unknown(model: &str, named_by: config::ModelOrigin) -> String {
    format!(
        "`{model}`, named by {}, cannot be reached — see examples/pi.toml",
        named_by.describe()
    )
}

/// Resolve a model name into something that can be talked to.
///
/// Startup and `/model` share this so the two cannot decide differently about
/// the same name. The config is the only way in: a model worth talking to is
/// worth four lines naming its endpoint and protocol, and every other field
/// already defaults to claiming nothing.
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

    // A passed-through model is a guess. Saying which guess lets the user
    // correct the one that matters instead of debugging a 400 later.
    let assumed = (!config.is_written(model)).then(|| {
        format!(
            "assuming a {}-token window, {} max output, no pricing, and no \
             thinking for `{}`. Set --context if the server's window differs; \
             --effort needs a config entry naming the model's thinking shape.",
            spec.context_window, spec.max_output_tokens, spec.model
        )
    });
    let key = config.key();
    let warning = config
        .api_key
        .as_deref()
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

/// The summarizer's own connection, when the config names a model for it: a
/// second dial, because a summary is a model call like any other and a cheaper
/// model for it is the point of the setting.
pub fn summary_writer(
    pinned: &Pinned,
    config: &config::Config,
    working: &str,
) -> Result<Option<(Arc<dyn Transport>, ModelSpec)>> {
    match &config.summarize_model {
        Some(name) if name != working => {
            let summarizer = dial(pinned, config, name, config::ModelOrigin::Global)
                .with_context(|| format!("summarize_model = \"{name}\""))?;
            Ok(Some((summarizer.transport, summarizer.spec)))
        }
        _ => Ok(None),
    }
}
