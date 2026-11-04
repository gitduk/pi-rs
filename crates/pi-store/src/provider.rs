//! Provider connections and the paths used to read their model catalogs.

use anyhow::{Context, Result, bail};
use llm::model::{CacheControl, Format};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::args::FormatArg;

/// A connection and a small field mapping, shared by all models on that host.
#[derive(Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub base_url: String,
    pub format: FormatArg,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default)]
    pub cache_control: CacheControl,
    /// An explicit catalog URL, when it is not at the connection's models path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_url: Option<String>,
    #[serde(default = "data")]
    pub models_path: String,
    #[serde(default = "id")]
    pub id: String,
    #[serde(default = "context_window")]
    pub context_window: String,
    #[serde(default = "max_output_tokens")]
    pub max_output_tokens: String,
}

impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Provider")
            .field("base_url", &self.base_url)
            .field("format", &self.format)
            .field("models_path", &self.models_path)
            .finish_non_exhaustive()
    }
}

fn data() -> String {
    "data".into()
}

fn id() -> String {
    "id".into()
}

fn context_window() -> String {
    "context_window".into()
}

fn max_output_tokens() -> String {
    "max_output_tokens".into()
}

fn at<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    if path.is_empty() {
        return Some(value);
    }
    path.split('.').try_fold(value, |value, key| value.get(key))
}

impl Provider {
    /// The protocol this connection speaks; independent of a model's name.
    pub fn wire_format(&self) -> Format {
        match self.format {
            FormatArg::Anthropic => Format::Anthropic {
                cache_control: self.cache_control,
            },
            FormatArg::Openai => Format::OpenAi,
            FormatArg::Chat => Format::Chat,
        }
    }

    /// The credential, with the same environment expansion as model keys.
    pub fn key(&self) -> Option<String> {
        self.api_key.as_deref().and_then(crate::config::expand_key)
    }

    /// Check paths at configuration load, before making any request.
    pub fn check(&self, name: &str) -> Result<()> {
        if name.is_empty() || name.contains('/') {
            bail!("provider names must be nonempty and contain no `/`: {name:?}");
        }
        for (field, path) in [
            ("models_path", &self.models_path),
            ("id", &self.id),
            ("context_window", &self.context_window),
            ("max_output_tokens", &self.max_output_tokens),
        ] {
            if (!path.is_empty() && path.split('.').any(str::is_empty))
                || path.chars().any(|c| "[]$*".contains(c))
            {
                bail!(
                    "provider.{name}.{field}: use dot-separated field names, such as `limit.context`"
                );
            }
        }
        if self.format != FormatArg::Anthropic && self.cache_control != CacheControl::Off {
            bail!("provider.{name}.cache_control requires format = \"anthropic\"");
        }
        Ok(())
    }

    /// Read each model independently, so missing facts cannot shift other rows.
    /// Returned entries contain only facts the endpoint actually supplied.
    pub fn read_models(&self, body: &Value) -> Result<toml::Table> {
        let rows = at(body, &self.models_path)
            .and_then(Value::as_array)
            .with_context(|| {
                format!(
                    "models_path {:?} does not select an array",
                    self.models_path
                )
            })?;
        let mut models = toml::Table::new();
        for (index, row) in rows.iter().enumerate() {
            let id = at(row, &self.id)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .with_context(|| {
                    format!("model {index}: path {:?} must contain a model ID", self.id)
                })?;
            let mut facts = toml::Table::new();
            for (field, path) in [
                ("context_window", &self.context_window),
                ("max_output_tokens", &self.max_output_tokens),
            ] {
                let Some(value) = at(row, path).filter(|v| !v.is_null()) else {
                    continue;
                };
                let number = value
                    .as_u64()
                    .or_else(|| value.as_str()?.parse().ok())
                    .filter(|n| *n > 0 && *n <= u64::from(u32::MAX))
                    .with_context(|| {
                        format!("model {id:?}: path {path:?} must contain a positive token count")
                    })?;
                facts.insert(field.into(), toml::Value::Integer(number as i64));
            }
            models.insert(id.into(), toml::Value::Table(facts));
        }
        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider(extra: &str) -> Provider {
        toml::from_str(&format!(
            "base_url = \":3425/v1\"\nformat = \"chat\"\n{extra}"
        ))
        .unwrap()
    }

    #[test]
    fn a_minimal_catalog_keeps_ids_and_leaves_missing_facts_unset() {
        let found = provider("")
            .read_models(&json!({"data": [
                {"id": "a", "context_window": 12345},
                {"id": "b"},
                {"id": "c", "max_output_tokens": "2048"}
            ]}))
            .unwrap();
        assert_eq!(found.len(), 3);
        assert_eq!(found["a"]["context_window"].as_integer(), Some(12345));
        assert!(found["b"].as_table().unwrap().is_empty());
        assert_eq!(found["c"]["max_output_tokens"].as_integer(), Some(2048));
    }

    #[test]
    fn array_and_model_paths_are_independent() {
        let p = provider(
            "models_path = \"result.models\"\nid = \"name\"\ncontext_window = \"limit.context\"\nmax_output_tokens = \"limit.output\"\n",
        );
        let found = p
            .read_models(&json!({"result": {"models": [
                {"name": "first", "limit": {"output": 2000}},
                {"name": "second", "limit": {"context": 200000}}
            ]}}))
            .unwrap();
        assert!(found["first"].get("context_window").is_none());
        assert_eq!(found["second"]["context_window"].as_integer(), Some(200000));
        assert!(found["second"].get("max_output_tokens").is_none());

        let root = provider("models_path = \"\"\n");
        assert!(
            root.read_models(&json!([{"id": "at-root"}]))
                .unwrap()
                .contains_key("at-root")
        );
    }

    #[test]
    fn malformed_paths_and_values_are_reported() {
        assert!(
            provider("models_path = \"data[]\"\n")
                .check("local")
                .is_err()
        );
        let p = provider("");
        assert!(p.read_models(&json!({"models": []})).is_err());
        assert!(
            p.read_models(&json!({"data": [{"name": "missing-id"}]}))
                .is_err()
        );
        assert!(
            p.read_models(&json!({"data": [{"id": "bad", "context_window": 0}]}))
                .is_err()
        );
    }

    #[test]
    fn token_counts_accept_both_ends_of_the_positive_u32_range() {
        let found = provider("")
            .read_models(&json!({"data": [{
                "id": "limits", "context_window": 1, "max_output_tokens": u32::MAX
            }]}))
            .unwrap();
        assert_eq!(found["limits"]["context_window"].as_integer(), Some(1));
        assert_eq!(
            found["limits"]["max_output_tokens"].as_integer(),
            Some(i64::from(u32::MAX))
        );
    }
}
