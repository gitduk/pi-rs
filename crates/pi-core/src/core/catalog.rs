//! Asynchronous discovery of the model facts described by provider settings.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::future;
use llm::model::Format;
use pi_store::config::{self, Config};
use pi_store::model_cache;
use pi_store::provider::Provider;
use pi_store::settings::Settings;
use tokio::sync::oneshot;

use super::Core;
use crate::args::Pinned;

// Bound a refresh and the time a tool waits for its result.
pub(super) const TIMEOUT: Duration = Duration::from_secs(5);

/// A snapshot of the discovered model facts applied to a resolved configuration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Generation(u64);

#[derive(Clone, PartialEq)]
struct Source {
    name: String,
    /// Connection settings with request credentials kept separately.
    provider: Provider,
    key: Option<String>,
    headers: Vec<(String, String)>,
}

impl Source {
    fn configured(pinned: &Pinned, config: &Config) -> Vec<Self> {
        config
            .provider
            .iter()
            .map(|(name, provider)| {
                let key = provider.key();
                let mut provider = provider.clone();
                let headers = provider.headers();
                provider.api_key = None;
                provider.headers.clear();
                if let Some(url) = &pinned.base_url {
                    provider.base_url = config::expand_base_url(url);
                }
                Self {
                    name: name.clone(),
                    provider,
                    key,
                    headers,
                }
            })
            .collect()
    }

    fn fingerprint(&self, cache: &model_cache::Cache) -> Result<String> {
        cache.fingerprint(
            &self.name,
            &self.provider,
            self.key.as_deref(),
            &self.headers,
        )
    }

    fn url(&self) -> String {
        self.provider.models_url.clone().unwrap_or_else(|| {
            let suffix = match self.provider.wire_format() {
                Format::Anthropic { .. } => "/v1/models",
                Format::OpenAi | Format::Chat => "/models",
            };
            format!("{}{suffix}", self.provider.base_url.trim_end_matches('/'))
        })
    }

    async fn read(&self) -> Result<toml::Table> {
        tokio::time::timeout(TIMEOUT, self.pages())
            .await
            .context("model discovery timed out")?
    }

    fn request(
        &self,
        client: &reqwest::Client,
        cursor: Option<&str>,
    ) -> Result<reqwest::RequestBuilder> {
        let mut url = reqwest::Url::parse(&self.url())?;
        if let Some(cursor) = cursor {
            url.query_pairs_mut().append_pair("after_id", cursor);
        }
        let mut request = client.get(url);
        for (name, value) in &self.headers {
            request = request.header(name, value);
        }
        if matches!(self.provider.wire_format(), Format::Anthropic { .. }) {
            request = request.header("anthropic-version", llm::transport::anthropic::API_VERSION);
            if let Some(key) = &self.key {
                request = request.header("x-api-key", key);
            }
        } else if let Some(key) = &self.key {
            request = request.bearer_auth(key);
        }
        Ok(request)
    }

    async fn page(
        &self,
        client: &reqwest::Client,
        cursor: Option<&str>,
    ) -> Result<serde_json::Value> {
        Ok(self
            .request(client, cursor)?
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    async fn pages(&self) -> Result<toml::Table> {
        let client = super::net::client()?;
        let mut models = toml::Table::new();
        let mut cursor: Option<String> = None;
        let mut seen = BTreeSet::new();
        loop {
            let body = self.page(&client, cursor.as_deref()).await?;
            models.extend(self.provider.read_models(&body)?);
            let Some(last) = next_cursor(&body)? else {
                break;
            };
            if !seen.insert(last.to_string()) {
                bail!("model discovery repeated pagination cursor {last:?}");
            }
            cursor = Some(last.to_string());
        }
        Ok(models)
    }
}

fn next_cursor(body: &serde_json::Value) -> Result<Option<&str>> {
    if body.get("has_more").and_then(serde_json::Value::as_bool) != Some(true) {
        return Ok(None);
    }
    body.get("last_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(Some)
        .context("has_more requires a last_id cursor")
}

struct Cached {
    source: Source,
    models: toml::Table,
}

type Results = Vec<(Source, Result<toml::Table, String>)>;

async fn read(sources: Vec<Source>) -> Results {
    future::join_all(sources.into_iter().map(|source| async move {
        let result = source
            .read()
            .await
            .map_err(|e| format!("provider {}: {e:#}", source.name));
        (source, result)
    }))
    .await
}

struct Pending {
    task: tokio::task::JoinHandle<()>,
    result: oneshot::Receiver<Results>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Last successful catalogs, retained while refreshes run or fail.
#[derive(Default)]
pub struct Catalog {
    cache: Option<model_cache::Cache>,
    generation: Generation,
    sources: Vec<Source>,
    known: BTreeMap<String, Cached>,
    pending: Option<Pending>,
    saving: Option<oneshot::Receiver<Result<(), String>>>,
    errors: Vec<String>,
}

struct Refresh {
    changed: bool,
    notes: Vec<String>,
}

impl Catalog {
    /// Reuse local model lists across invocations without contacting providers.
    pub fn persistent() -> Self {
        Self {
            cache: model_cache::Cache::local(),
            ..Self::default()
        }
    }

    /// Publish discovery readiness to the tools of a newly resolved run.
    pub(super) fn publish_status(&self, resolved: &super::resolve::Resolved) {
        resolved.models.discovery(self.discovery());
    }
    fn discovery(&self) -> super::models::Discovery {
        use super::models::Discovery;
        if self.sources.is_empty() {
            Discovery::NotConfigured
        } else if self.pending.is_some() {
            Discovery::Pending
        } else if !self.errors.is_empty() {
            Discovery::Failed
        } else {
            Discovery::Ready
        }
    }
    /// The version of the last successfully changed catalog.
    pub fn generation(&self) -> Generation {
        self.generation
    }
    /// Apply only facts belonging to the connections that are still configured.
    pub fn enrich(&self, settings: &Settings, pinned: &Pinned, config: Config) -> Result<Config> {
        let mut models = toml::Table::new();
        let mut used_disk = false;
        let mut snapshot = None;
        for source in Source::configured(pinned, &config) {
            let saved;
            let facts = if let Some(cached) = self
                .known
                .get(&source.name)
                .filter(|cached| cached.source == source)
            {
                &cached.models
            } else if let Some(found) = self.cache.as_ref().and_then(|cache| {
                let identity = source.fingerprint(cache).ok()?;
                snapshot
                    .get_or_insert_with(|| cache.snapshot())
                    .get(&source.name, &identity)
            }) {
                used_disk = true;
                saved = found;
                &saved
            } else {
                continue;
            };
            models.extend(
                facts
                    .iter()
                    .map(|(id, facts)| (format!("{}/{id}", source.name), facts.clone())),
            );
        }
        if models.is_empty() {
            return Ok(config);
        }
        match settings.config_with_models(&models) {
            Err(error) if used_disk => {
                tracing::warn!(%error, "could not apply cached model catalogs");
                let mut live = toml::Table::new();
                for source in Source::configured(pinned, &config) {
                    if let Some(cached) = self
                        .known
                        .get(&source.name)
                        .filter(|cached| cached.source == source)
                    {
                        live.extend(
                            cached.models.iter().map(|(id, facts)| {
                                (format!("{}/{id}", source.name), facts.clone())
                            }),
                        );
                    }
                }
                if live.is_empty() {
                    Ok(config)
                } else {
                    settings.config_with_models(&live)
                }
            }
            result => result,
        }
    }

    /// Start an explicitly requested discovery, retaining the current catalog.
    pub(super) fn refresh(&mut self, pinned: &Pinned, config: &Config) {
        self.configure(pinned, config);
        if self.awaiting_models() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let sources = self.sources.clone();
        let (send, result) = oneshot::channel();
        let task = runtime.spawn(async move {
            let _ = send.send(read(sources).await);
        });
        self.pending = Some(Pending { task, result });
    }

    fn accept(&mut self, results: Results) -> Refresh {
        let mut changed = false;
        let mut errors = Vec::new();
        self.save(&results);
        for (source, result) in results {
            match result {
                Ok(models) => {
                    let moved = self
                        .known
                        .get(&source.name)
                        .is_none_or(|cached| cached.source != source || cached.models != models);
                    if moved {
                        changed = true;
                        self.known
                            .insert(source.name.clone(), Cached { source, models });
                    }
                }
                Err(error) => errors.push(error),
            }
        }
        let notes = errors
            .iter()
            .filter(|e| !self.errors.contains(e))
            .map(|e| format!("model discovery unavailable — {e}"))
            .collect();
        self.errors = errors;
        if changed {
            self.generation.0 += 1;
        }
        Refresh { changed, notes }
    }

    fn save(&mut self, results: &Results) {
        let Some(cache) = self.cache.clone() else {
            return;
        };
        let lists: Vec<_> = results
            .iter()
            .map(|(source, result)| {
                let models = result.as_ref().ok().or_else(|| {
                    self.known
                        .get(&source.name)
                        .filter(|cached| cached.source == *source)
                        .map(|cached| &cached.models)
                });
                (source.clone(), models.cloned())
            })
            .collect();
        let (send, result) = oneshot::channel();
        tokio::task::spawn_blocking(move || {
            let save = || -> Result<()> {
                let lists = lists
                    .iter()
                    .map(|(source, models)| {
                        Ok((
                            source.name.clone(),
                            source.fingerprint(&cache)?,
                            models.as_ref(),
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                cache.write(lists)
            };
            let _ = send.send(save().map_err(|error| format!("{error:#}")));
        });
        self.saving = Some(result);
    }

    fn configure(&mut self, pinned: &Pinned, config: &Config) {
        let sources = Source::configured(pinned, config);
        if self.sources != sources {
            self.pending = None;
            self.known
                .retain(|_, cached| sources.contains(&cached.source));
            self.errors.clear();
        }
        self.sources = sources;
    }

    fn finish_pending(&mut self) -> Option<Refresh> {
        match self.pending.as_mut()?.result.try_recv() {
            Ok(results) => {
                self.pending = None;
                Some(self.accept(results))
            }
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => {
                self.pending = None;
                Some(Refresh {
                    changed: false,
                    notes: vec!["model discovery ended without a result".into()],
                })
            }
        }
    }

    /// A list nobody has answered yet: a surface keeps waking for it, so its
    /// facts land when they arrive rather than at the next keypress.
    pub fn awaiting_models(&self) -> bool {
        self.pending.is_some() || self.saving.is_some()
    }

    fn poll(&mut self, pinned: &Pinned, config: &Config) -> Option<Refresh> {
        self.configure(pinned, config);
        if let Some(saving) = &mut self.saving {
            let error = match saving.try_recv() {
                Ok(result) => result.err(),
                Err(oneshot::error::TryRecvError::Empty) => return None,
                Err(oneshot::error::TryRecvError::Closed) => {
                    Some("the write ended without a result".into())
                }
            };
            self.saving = None;
            return error.map(|error| Refresh {
                changed: false,
                notes: vec![format!("model catalog cache could not be saved — {error}")],
            });
        }
        self.finish_pending()
    }
}

impl Core {
    /// Whether a model list is still on its way.
    pub fn awaiting_models(&self) -> bool {
        self.catalog.awaiting_models()
    }

    /// Poll discovery without blocking a caller; new facts rebuild the active agent.
    pub fn refresh_models(&mut self) -> Option<Vec<String>> {
        let config = self.config().clone();
        let Some(refresh) = self.catalog.poll(&self.pinned, &config) else {
            self.catalog.publish_status(self.lane().resolved());
            return None;
        };
        let mut notes = refresh.notes;
        for note in &notes {
            tracing::warn!(target: "pi::session", reason = %note, "model discovery failed");
        }
        if refresh.changed {
            match self.rebuilt(None) {
                Ok(more) => {
                    notes.extend(more);
                    notes.push(format!(
                        "models refreshed — {} available",
                        self.config().names().len()
                    ));
                }
                Err(why) => notes.push(why),
            }
        } else if !notes.is_empty() {
            let spec = self.lane().agent().spec();
            notes.extend(crate::core::dial::assumed(
                &self.pinned,
                self.config(),
                spec,
            ));
        }
        self.catalog.publish_status(self.lane().resolved());
        (!notes.is_empty()).then_some(notes)
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};

    use serde_json::{Value, json};

    use super::*;

    fn server(replies: Vec<(u16, Value)>) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (send, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (status, body) in replies {
                let (stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream);
                let mut request = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse().unwrap();
                    }
                    request.push_str(&line);
                }
                std::io::Read::read_exact(&mut reader, &mut vec![0; length]).unwrap();
                let _ = send.send(request);
                let body = body.to_string();
                write!(reader.into_inner(),
                    "HTTP/1.1 {status} result\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        (format!("http://{address}/v1"), received)
    }

    fn settings(url: &str) -> Settings {
        Settings::new(
            toml::from_str(&format!(
                r#"
[provider.test]
base_url = "{url}"
format = "chat"
api_key = "test-credential"
models_path = "result.models"
id = "name"
context_window = "limit.context"
max_output_tokens = "limit.output"
[provider.test.headers]
x-opencode-session = "test-session"
[models."test/a"]
max_output_tokens = 2048
"#
            ))
            .unwrap(),
            None,
        )
    }

    async fn refreshed(catalog: &mut Catalog, pinned: &Pinned, config: &Config) -> Refresh {
        catalog.refresh(pinned, config);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(result) = catalog.poll(pinned, config) {
                    break result;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }

    async fn two_lanes(root: &std::path::Path, file: &std::path::Path, discover: bool) -> Core {
        use crate::core::lane::{Lane, Opening, a_resolved};
        use std::sync::Arc;

        let pinned = Pinned {
            config: Some(file.display().to_string()),
            no_skills: true,
            no_context_files: true,
            ..Default::default()
        };
        let settings = Settings::load(pinned.config.as_deref(), root).unwrap();
        let config = settings.config().unwrap();
        let mut catalog = Catalog::default();
        if discover {
            assert!(
                refreshed(&mut catalog, &pinned, &config)
                    .await
                    .notes
                    .is_empty()
            );
        }
        let config = catalog.enrich(&settings, &pinned, config).unwrap();
        let dialled =
            crate::core::dial::dial(&pinned, &config, "local/m", config::ModelOrigin::Global)
                .unwrap();
        let agent = Arc::new(agent::Agent::new(dialled.transport, dialled.spec));
        let config = Arc::new(config);
        let lanes = ["one", "two"]
            .into_iter()
            .map(|name| {
                let root = root.join(name);
                std::fs::create_dir_all(root.join(".git")).unwrap();
                let resolved = a_resolved("");
                let environment = crate::core::environment::Environment::new(
                    config.clone(),
                    settings.clone(),
                    resolved.commands.clone(),
                    crate::core::mcp::Live::default(),
                );
                Lane::opened(Opening::new(
                    agent.clone(),
                    resolved,
                    tool::Ctx::new(tool::Workspace::new(root).unwrap()),
                    environment,
                ))
            })
            .collect::<Vec<_>>();
        let mut lanes = lanes.into_iter();
        let mut ring = crate::core::lanes::Lanes::of(lanes.next().expect("a first lane"));
        ring.push(lanes.next().expect("a second lane"));
        let mut core = Core {
            runtime: Default::default(),
            store: pi_store::session::Store::new(root.join("state")),
            catalog,
            pinned,
            lanes: ring,
            executions: None,
            services: None,
            naming: false,
            removals: Default::default(),
            trees: None,
            surface: &[],
        };
        core.try_reload().unwrap();
        core.lanes.show(1);
        core.try_reload().unwrap();
        core
    }

    #[tokio::test]
    async fn each_lane_installs_the_rotated_provider_credential() {
        use llm::Transport as _;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.toml");
        let (url, requests) = server(vec![(200, json!({}))]);
        let body = |key| {
            format!(
                r#"
[provider.local]
base_url = "{url}"
format = "chat"
api_key = "{key}"
[models."local/m"]
context_window = 128000
"#
            )
        };
        std::fs::write(&file, body("old-test-key")).unwrap();
        let mut core = two_lanes(dir.path(), &file, false).await;
        std::fs::write(&file, body("new-test-key")).unwrap();
        core.try_reload().unwrap();
        core.lanes.show(0);
        core.refresh_config().unwrap();

        let agent = core.lane().agent();
        let _ = agent
            .wire()
            .stream(agent.spec(), &llm::Request::default())
            .await;
        let request = requests
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .to_ascii_lowercase();
        assert!(
            request.contains("authorization: bearer new-test-key"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn a_lane_applies_new_catalog_facts_without_a_file_edit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.toml");
        let catalog = |window| json!({"data": [{"id": "m", "context_window": window}]});
        let (url, _) = server(vec![(200, catalog(100000)), (200, catalog(300000))]);
        std::fs::write(
            &file,
            format!("[provider.local]\nbase_url = \"{url}\"\nformat = \"chat\"\n"),
        )
        .unwrap();
        let mut core = two_lanes(dir.path(), &file, true).await;
        core.dispatch(crate::input::Intent::Builtin(
            crate::input::Builtin::RefreshModels,
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if core.refresh_models().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(core.lane().agent().spec().context_window, 300000);
        core.lanes.show(0);
        let said = core.refresh_model_config().unwrap();
        assert!(
            said.is_empty(),
            "applying facts alone says nothing: {said:?}"
        );
        assert_eq!(core.lane().agent().spec().context_window, 300000);
        assert!(
            core.refresh_model_config().is_none(),
            "the current generation is already applied"
        );
    }

    #[tokio::test]
    async fn discovery_reads_mapped_pages_and_sends_the_provider_credential() {
        let (url, requests) = server(vec![
            (
                200,
                json!({"result": {"models": [{"name": "a", "limit": {"context": 200000, "output": 10000}}]}, "has_more": true, "last_id": "a/b"}),
            ),
            (
                200,
                json!({"result": {"models": [{"name": "b"}]}, "has_more": false}),
            ),
        ]);
        let settings = settings(&url);
        let pinned = Pinned::default();
        let mut catalog = Catalog::default();
        assert!(
            refreshed(&mut catalog, &pinned, &settings.config().unwrap())
                .await
                .notes
                .is_empty()
        );
        let config = catalog
            .enrich(&settings, &pinned, settings.config().unwrap())
            .unwrap();
        assert_eq!(config.names(), ["test/a", "test/b"]);
        let a = config.find("test/a").unwrap();
        assert_eq!(a.context_window, 200000);
        assert_eq!(a.max_output_tokens, 2048, "manual fields win");
        assert_eq!(config.find("test/b").unwrap().context_window, 128000);
        let first = requests.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(first.starts_with("GET /v1/models "), "{first}");
        assert!(
            first
                .to_ascii_lowercase()
                .contains("authorization: bearer test-credential")
        );
        assert!(
            first
                .to_ascii_lowercase()
                .contains("x-opencode-session: test-session"),
            "{first}"
        );
        let second = requests.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            second.starts_with("GET /v1/models?after_id=a%2Fb "),
            "{second}"
        );
    }

    #[tokio::test]
    async fn failed_refresh_keeps_the_last_catalog_and_reports_the_same_error_once() {
        let (url, _) = server(vec![
            (
                200,
                json!({"result": {"models": [{"name": "a", "limit": {"context": 222222}}]}}),
            ),
            (503, json!({"error": "try later"})),
            (503, json!({"error": "try later"})),
        ]);
        let settings = settings(&url);
        let pinned = Pinned::default();
        let config = settings.config().unwrap();
        let mut catalog = Catalog::default();
        assert!(
            refreshed(&mut catalog, &pinned, &config)
                .await
                .notes
                .is_empty()
        );
        assert_eq!(
            refreshed(&mut catalog, &pinned, &config).await.notes.len(),
            1
        );
        assert!(
            refreshed(&mut catalog, &pinned, &config)
                .await
                .notes
                .is_empty()
        );
        let config = catalog.enrich(&settings, &pinned, config).unwrap();
        assert_eq!(config.find("test/a").unwrap().context_window, 222222);

        let elsewhere = Settings::new(
            toml::from_str(
                r#"
[provider.test]
base_url = "http://127.0.0.1:1/v1"
format = "chat"
"#,
            )
            .unwrap(),
            None,
        );
        let config = catalog
            .enrich(&elsewhere, &pinned, elsewhere.config().unwrap())
            .unwrap();
        assert!(
            config.names().is_empty(),
            "another connection must not inherit these facts"
        );
    }

    #[tokio::test]
    async fn a_failed_round_waits_for_an_explicit_refresh() {
        use crate::core::models::Discovery;

        let (url, _) = server(vec![
            (503, json!({"error": "try later"})),
            (
                200,
                json!({"result": {"models": [{"name": "a", "limit": {"context": 222222}}]}}),
            ),
        ]);
        let pinned = Pinned::default();
        let config = settings(&url).config().unwrap();
        let mut catalog = Catalog::default();
        assert_eq!(
            refreshed(&mut catalog, &pinned, &config).await.notes.len(),
            1
        );
        assert!(catalog.poll(&pinned, &config).is_none());
        assert!(!catalog.awaiting_models());
        assert_eq!(catalog.discovery(), Discovery::Failed);

        let refresh = refreshed(&mut catalog, &pinned, &config).await;
        assert!(refresh.changed);
        assert_eq!(catalog.discovery(), Discovery::Ready);
    }

    #[tokio::test]
    async fn refresh_runs_in_the_background_and_replaces_changed_facts() {
        let (url, _) = server(vec![
            (
                200,
                json!({"result": {"models": [{"name": "a", "limit": {"context": 100000}}]}}),
            ),
            (
                200,
                json!({"result": {"models": [{"name": "b", "limit": {"context": 300000}}]}}),
            ),
        ]);
        let settings = settings(&url);
        let pinned = Pinned::default();
        let config = settings.config().unwrap();
        let mut catalog = Catalog::default();
        assert!(refreshed(&mut catalog, &pinned, &config).await.changed);
        assert!(
            catalog.poll(&pinned, &config).is_none(),
            "a list is read again only when asked"
        );
        let refreshed = refreshed(&mut catalog, &pinned, &config).await;
        assert!(refreshed.changed);
        let config = catalog.enrich(&settings, &pinned, config).unwrap();
        assert_eq!(
            config.names(),
            ["test/a", "test/b"],
            "the manual entry remains"
        );
        assert_eq!(
            config.find("test/a").unwrap().context_window,
            128000,
            "removed facts are gone"
        );
        assert_eq!(config.find("test/b").unwrap().context_window, 300000);
    }

    #[test]
    fn a_fact_on_an_alternative_model_updates_the_catalog() {
        let mut catalog = Catalog::default();
        assert!(
            catalog
                .accept(vec![answer("test", &[("a", 100000), ("b", 9000)])])
                .changed
        );
        let first = catalog.generation();

        assert!(
            catalog
                .accept(vec![answer("test", &[("a", 100000), ("b", 8000)])])
                .changed,
            "alternative model facts must reach selection queries"
        );
        assert_ne!(catalog.generation(), first);
        let current = catalog.generation();

        assert!(
            !catalog
                .accept(vec![answer("test", &[("a", 100000), ("b", 8000)])])
                .changed,
            "identical facts do not rebuild the catalog"
        );
        assert_eq!(catalog.generation(), current);
    }

    fn answer(name: &str, rows: &[(&str, i64)]) -> (Source, Result<toml::Table, String>) {
        let source = Source {
            name: name.into(),
            provider: toml::from_str("base_url = \"https://example.test/v1\"\nformat = \"chat\"")
                .unwrap(),
            key: None,
            headers: Vec::new(),
        };
        let mut models = toml::Table::new();
        for (id, window) in rows {
            let mut facts = toml::Table::new();
            facts.insert("context_window".into(), toml::Value::Integer(*window));
            models.insert((*id).into(), toml::Value::Table(facts));
        }
        (source, Ok(models))
    }

    #[tokio::test]
    async fn a_rotated_credential_invalidates_the_previous_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.toml");
        let (url, _) = server(vec![(
            200,
            json!({"data": [{"id": "m", "context_window": 200000}]}),
        )]);
        let body = |key: &str| {
            format!(
                "[provider.local]\nbase_url = \"{url}\"\nformat = \"chat\"\napi_key = \"{key}\"\n\
                 [models.\"local/m\"]\nmax_output_tokens = 2048\n"
            )
        };
        std::fs::write(&file, body("first-key")).unwrap();
        let mut core = two_lanes(dir.path(), &file, true).await;
        assert_eq!(core.lane().agent().spec().context_window, 200000);

        std::fs::write(&file, body("second-key")).unwrap();
        core.try_reload().unwrap();
        assert_eq!(
            core.lane().agent().spec().context_window,
            128000,
            "the new credential cannot inherit facts read under the old credential"
        );
    }

    #[tokio::test]
    async fn a_lane_opened_after_discovery_is_already_current() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.toml");
        let (url, _) = server(vec![(
            200,
            json!({"data": [{"id": "m", "context_window": 100000}]}),
        )]);
        std::fs::write(
            &file,
            format!("[provider.local]\nbase_url = \"{url}\"\nformat = \"chat\"\n"),
        )
        .unwrap();
        let mut core = two_lanes(dir.path(), &file, true).await;
        assert_ne!(core.catalog.generation(), Generation::default());

        let root = dir.path().join("three");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        core.open_lane(tool::Workspace::new(root).unwrap(), None, 0)
            .unwrap();
        let resolved = core.lane().resolved();
        assert!(resolved.notes.is_empty(), "{:?}", resolved.notes);
        assert_eq!(resolved.catalog_generation, core.catalog.generation());
        assert!(core.refresh_model_config().is_none());
    }

    // A failed refresh reports what happened and what facts are still assumed.
    #[tokio::test]
    async fn a_list_that_never_comes_names_what_is_assumed_instead() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.toml");
        let (url, _) = server(vec![(500, json!({"error": "no list"}))]);
        std::fs::write(
            &file,
            format!("[provider.local]\nbase_url = \"{url}\"\nformat = \"chat\"\n"),
        )
        .unwrap();
        let mut core = two_lanes(dir.path(), &file, false).await;
        core.dispatch(crate::input::Intent::Builtin(
            crate::input::Builtin::RefreshModels,
        ));
        let notes = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(notes) = core.refresh_models() {
                    break notes;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            notes.iter().any(|n| n.contains("`local/m`")),
            "the model whose facts nobody stated is named: {notes:?}"
        );
        assert!(
            notes.len() >= 2 && notes.iter().any(|n| n.contains("local")),
            "the failure itself is said too: {notes:?}"
        );
    }
}
