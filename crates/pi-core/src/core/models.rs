//! Model selection facts exposed without connection secrets.

use async_trait::async_trait;
use pi_store::args::EffortArg;
use pi_store::config::Config;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::watch;
use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};

#[derive(Clone)]
pub(crate) struct Models {
    state: watch::Sender<Snapshot>,
}

#[derive(Clone)]
struct Snapshot {
    catalog: Value,
    count: usize,
    config: Arc<Config>,
    discovery: Discovery,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Discovery {
    Pending,
    Ready,
    Failed,
    NotConfigured,
}

impl Models {
    pub(super) fn new(config: &Config) -> Self {
        Self {
            state: watch::channel(Snapshot::new(config)).0,
        }
    }

    pub(super) fn config(&self) -> Arc<Config> {
        self.state.borrow().config.clone()
    }

    pub(super) fn replace(&self, next: &Self) {
        let snapshot = next.state.borrow().clone();
        self.state.send_replace(snapshot);
    }

    pub(super) fn discovery(&self, discovery: Discovery) {
        self.state.send_if_modified(|snapshot| {
            if snapshot.discovery == discovery {
                return false;
            }
            snapshot.discovery = discovery;
            true
        });
    }
}

impl Snapshot {
    fn new(config: &Config) -> Self {
        let mut names = config.names();
        if let Some(name) = &config.model
            && let Ok(spec) = config.find(name)
        {
            names.push(spec.model);
        }
        names.sort();
        names.dedup();
        let models: Vec<Value> = names
            .into_iter()
            .map(|id| {
                let entry = config.entry(&id);
                let rate = |value: f64| (value.is_finite() && value > 0.0).then_some(value);
                json!({
                    "id": id,
                    "context_window": entry.context_window,
                    "max_output_tokens": entry.max_output_tokens,
                    // These defaults do not establish remote model capabilities.
                    "vision": entry.vision.then_some(true),
                    "accepts_effort": entry.effort.map(|effort| effort != EffortArg::Unsupported),
                    "pricing": {
                        "input_per_mtok": rate(entry.pricing.input_per_mtok),
                        "output_per_mtok": rate(entry.pricing.output_per_mtok),
                        "cache_read_per_mtok": rate(entry.pricing.cache_read_per_mtok),
                        "cache_write_per_mtok": rate(entry.pricing.cache_write_per_mtok),
                    },
                })
            })
            .collect();
        Self {
            count: models.len(),
            config: Arc::new(Config {
                provider: config.provider.clone(),
                models: config.models.clone(),
                discovered: config.discovered.clone(),
                model: config.model.clone(),
                ..Default::default()
            }),
            discovery: if config.provider.is_empty() {
                Discovery::NotConfigured
            } else {
                Discovery::Ready
            },
            catalog: json!({
                "models": models,
                "notes": "This is the locally known catalog, not a live availability check. null means unknown or unconfirmed. Prices are per million tokens. Zero prices are unconfirmed because the catalog also uses zero for missing rates. Model names do not establish speed or task suitability.",
            }),
        }
    }
}

impl tool::Source for Models {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(self.clone())]
    }
}

#[async_trait]
impl Tool for Models {
    fn name(&self) -> &str {
        "models"
    }

    fn description(&self) -> &str {
        "List locally known model IDs, limits, capabilities, and prices without network requests or connection secrets. \
         Use the returned IDs when selecting a model. \
         Null fields are unknown. Model names do not establish task suitability. \
         If discovery remains pending, query again later."
    }

    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {}
        let _: Args = tool::parse_args(args)?;
        let mut state = self.state.subscribe();
        let wait = async {
            loop {
                if state.borrow_and_update().discovery != Discovery::Pending {
                    break;
                }
                if state.changed().await.is_err() {
                    break;
                }
            }
        };
        tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            _ = tokio::time::timeout(super::catalog::TIMEOUT, wait) => {}
        }
        let snapshot = state.borrow();
        let mut catalog = snapshot.catalog.clone();
        catalog["discovery"] = json!(snapshot.discovery);
        Ok(ToolOutput::text(catalog.to_string())
            .with_screen(format!("{} known models", snapshot.count)))
    }
}
