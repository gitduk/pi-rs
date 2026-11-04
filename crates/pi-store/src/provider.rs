//! Provider connections and the paths used to read their model catalogs.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use llm::model::{CacheControl, Format};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::args::FormatArg;

/// What a catalog quotes its rates per: a million tokens, or one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PriceUnit {
    #[default]
    Mtok,
    Token,
}

impl PriceUnit {
    /// The quoted rate as the million-token figure a model entry carries.
    fn per_mtok(self, quoted: f64) -> f64 {
        match self {
            Self::Mtok => quoted,
            Self::Token => quoted * 1_000_000.0,
        }
    }
}

/// Paths to the rates a catalog quotes, and the unit they are quoted in.
#[derive(Clone, Debug, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PricePaths {
    /// `mtok` unless the catalog quotes a per-token figure.
    #[serde(default)]
    pub unit: PriceUnit,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_mtok: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_mtok: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_mtok: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_mtok: Option<String>,
}

impl PricePaths {
    fn is_empty(&self) -> bool {
        self.unit == PriceUnit::Mtok
            && self.input_per_mtok.is_none()
            && self.output_per_mtok.is_none()
            && self.cache_read_per_mtok.is_none()
            && self.cache_write_per_mtok.is_none()
    }
}

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
    /// Where the catalog says images are accepted: a bool, or a list of input
    /// types where any `image` or `image/*` counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<String>,
    /// Where the catalog says the model takes a temperature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepts_temperature: Option<String>,
    /// Where the catalog says the model reasons; `false` means no effort level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Where the catalog quotes its rates; a pair left unset stays zero.
    #[serde(default, skip_serializing_if = "PricePaths::is_empty")]
    pub pricing: PricePaths,
    /// Sent with every request; a leading `$` reads the environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// The header this host takes a conversation id in. Absent: no request
    /// names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_header: Option<String>,
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

// A token count is stated as a number or a numeric string; anything else is
// a fact the endpoint did not supply.
fn tokens(value: &Value) -> Option<i64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|n| *n > 0 && *n <= u64::from(u32::MAX))
        .map(|n| n as i64)
}

fn accepts_images(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(accepts) => Some(*accepts),
        Value::Array(kinds) => {
            let mut any_image = false;
            for kind in kinds {
                let kind = kind.as_str()?;
                any_image |= kind == "image" || kind.starts_with("image/");
            }
            Some(any_image)
        }
        _ => None,
    }
}

// RFC 9110: a field name is a token, and a field value is printable ASCII.
fn header_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn header_value_ok(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
}

fn rate(value: &Value) -> Option<f64> {
    let rate = value.as_f64().or_else(|| value.as_str()?.parse().ok())?;
    (rate.is_finite() && rate >= 0.0).then_some(rate)
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

    /// The headers as sent, read the same way: one whose variable is unset is
    /// left out rather than sent empty.
    pub fn headers(&self) -> Vec<(String, String)> {
        crate::config::expanded(&self.headers)
    }

    /// Check paths at configuration load, before making any request.
    pub fn check(&self, name: &str) -> Result<()> {
        if name.is_empty() || name.contains('/') {
            bail!("provider names must be nonempty and contain no `/`: {name:?}");
        }
        for (field, path) in [
            ("models_path", Some(self.models_path.as_str())),
            ("id", Some(self.id.as_str())),
            ("context_window", Some(self.context_window.as_str())),
            ("max_output_tokens", Some(self.max_output_tokens.as_str())),
            ("vision", self.vision.as_deref()),
            ("accepts_temperature", self.accepts_temperature.as_deref()),
            ("reasoning", self.reasoning.as_deref()),
            (
                "pricing.input_per_mtok",
                self.pricing.input_per_mtok.as_deref(),
            ),
            (
                "pricing.output_per_mtok",
                self.pricing.output_per_mtok.as_deref(),
            ),
            (
                "pricing.cache_read_per_mtok",
                self.pricing.cache_read_per_mtok.as_deref(),
            ),
            (
                "pricing.cache_write_per_mtok",
                self.pricing.cache_write_per_mtok.as_deref(),
            ),
        ] {
            let Some(path) = path else { continue };
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
        // Names are checked as written: an unset variable drops the entry, and
        // would otherwise hide a bad name until the variable exists.
        for header in self.headers.keys() {
            if !header_name_ok(header) {
                bail!(
                    "provider.{name}.headers.{header:?}: a header name is letters, digits or !#$%&'*+-.^_`|~"
                );
            }
        }
        for (header, value) in self.headers() {
            if !header_value_ok(&value) {
                bail!(
                    "provider.{name}.headers.{header:?}: a header value is printable ASCII, tabs allowed"
                );
            }
        }
        if let Some(header) = &self.session_header
            && !header_name_ok(header)
        {
            bail!(
                "provider.{name}.session_header: a header name is letters, digits or !#$%&'*+-.^_`|~"
            );
        }
        Ok(())
    }

    /// Read each model independently: a row without an ID is skipped, and a
    /// fact the endpoint states unreadably is left unset, so one odd row
    /// cannot discard the rest. Entries hold only facts the endpoint supplied.
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
        for row in rows {
            let Some(id) = at(row, &self.id)
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
            else {
                continue;
            };
            models.insert(id.into(), toml::Value::Table(self.facts(row)));
        }
        if models.is_empty() && !rows.is_empty() {
            bail!(
                "path {:?} selected no model ID among {} rows",
                self.id,
                rows.len()
            );
        }
        Ok(models)
    }

    fn facts(&self, row: &Value) -> toml::Table {
        let mut facts = toml::Table::new();
        for (field, path) in [
            ("context_window", &self.context_window),
            ("max_output_tokens", &self.max_output_tokens),
        ] {
            if let Some(count) = at(row, path).and_then(tokens) {
                facts.insert(field.into(), toml::Value::Integer(count));
            }
        }
        if let Some(vision) = self
            .vision
            .as_deref()
            .and_then(|path| at(row, path))
            .and_then(accepts_images)
        {
            facts.insert("vision".into(), toml::Value::Boolean(vision));
        }
        if let Some(accepts) = self
            .accepts_temperature
            .as_deref()
            .and_then(|path| at(row, path))
            .and_then(Value::as_bool)
        {
            facts.insert("accepts_temperature".into(), toml::Value::Boolean(accepts));
        }
        if self
            .reasoning
            .as_deref()
            .and_then(|path| at(row, path))
            .and_then(Value::as_bool)
            == Some(false)
        {
            facts.insert("effort".into(), toml::Value::String("unsupported".into()));
        }
        let mut pricing = toml::Table::new();
        for (field, path) in [
            ("input_per_mtok", &self.pricing.input_per_mtok),
            ("output_per_mtok", &self.pricing.output_per_mtok),
            ("cache_read_per_mtok", &self.pricing.cache_read_per_mtok),
            ("cache_write_per_mtok", &self.pricing.cache_write_per_mtok),
        ] {
            let Some(quoted) = path
                .as_deref()
                .and_then(|path| at(row, path))
                .and_then(rate)
            else {
                continue;
            };
            pricing.insert(
                field.into(),
                toml::Value::Float(self.pricing.unit.per_mtok(quoted)),
            );
        }
        if !pricing.is_empty() {
            facts.insert("pricing".into(), toml::Value::Table(pricing));
        }
        facts
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
    fn a_session_header_is_a_header_name_or_nothing() {
        assert_eq!(
            provider("session_header = \"x-opencode-session\"\n").session_header,
            Some("x-opencode-session".into())
        );
        assert!(
            provider("session_header = \"x session\"\n")
                .check("local")
                .is_err()
        );
        assert!(provider("").session_header.is_none());
    }

    #[test]
    fn malformed_headers_are_refused_at_load() {
        for bad in [
            "[headers]\n\"x session\" = \"v\"\n",
            "[headers]\nx-ok = \"line\\nbreak\"\n",
            "[headers]\nx-ok = \"日本語\"\n",
            "[headers]\n\"x session\" = \"$PI_TEST_HEADER_UNSET\"\n",
        ] {
            assert!(provider(bad).check("local").is_err(), "accepted: {bad}");
        }
        assert!(
            provider("[headers]\nx-session = \"$HOME\"\n")
                .check("local")
                .is_ok()
        );
    }

    #[test]
    fn headers_read_the_environment_and_drop_unset_variables() {
        let p = provider("[headers]\nx-home = \"$HOME\"\nx-missing = \"$PI_TEST_HEADER_UNSET\"\n");
        let headers = p.headers();
        assert_eq!(headers.len(), 1, "{headers:?}");
        assert_eq!(headers[0].0, "x-home");
        assert_eq!(headers[0].1, std::env::var("HOME").unwrap());
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
    fn malformed_paths_are_refused_at_load() {
        for bad in [
            "models_path = \"data[]\"\n",
            "models_path = \"data..id\"\n",
            "vision = \"modalities.input[]\"\n",
            "pricing = { input_per_mtok = \"cost[]\" }\n",
        ] {
            assert!(provider(bad).check("local").is_err(), "accepted: {bad}");
        }
        assert!(
            provider("vision = \"modalities.input\"\n")
                .check("local")
                .is_ok()
        );
    }

    #[test]
    fn an_id_path_that_selects_nothing_is_an_error() {
        let p = provider("");
        assert!(p.read_models(&json!({"models": []})).is_err(), "no array");
        assert!(
            p.read_models(&json!({"data": [{"name": "missing-id"}]}))
                .is_err(),
            "a wrong id path must not read as an empty catalog"
        );
        assert!(
            p.read_models(&json!({"data": []})).unwrap().is_empty(),
            "an empty catalog is not a mistake"
        );
    }

    #[test]
    fn one_unreadable_row_does_not_discard_the_rest() {
        let found = provider("")
            .read_models(&json!({"data": [
                {"id": "good", "context_window": 1000},
                {"name": "no-id"},
                {"id": "odd", "context_window": 0},
                {"id": "", "context_window": 2000}
            ]}))
            .unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found["good"]["context_window"].as_integer(), Some(1000));
        assert!(
            found["odd"].as_table().unwrap().is_empty(),
            "an unreadable count leaves that fact unset"
        );
    }

    #[test]
    fn a_modality_named_image_counts_as_vision() {
        let found = provider("vision = \"modalities.input\"\n")
            .read_models(&json!({"data": [
                {"id": "a", "modalities": {"input": ["text", "image"]}},
                {"id": "b", "modalities": {"input": ["text", "pdf"]}}
            ]}))
            .unwrap();
        assert_eq!(found["a"]["vision"].as_bool(), Some(true));
        assert_eq!(found["b"]["vision"].as_bool(), Some(false));
    }

    #[test]
    fn per_token_rates_arrive_per_million() {
        let p = provider("[pricing]\nunit = \"token\"\ninput_per_mtok = \"pricing.prompt\"\n");
        let found = p
            .read_models(&json!({"data": [{"id": "a", "pricing": {"prompt": "0.00000015"}}]}))
            .unwrap();
        let rate = found["a"]["pricing"]["input_per_mtok"].as_float().unwrap();
        assert!((rate - 0.15).abs() < 1e-9, "{rate}");
    }

    #[test]
    fn capability_and_price_facts_follow_their_paths() {
        let p = provider(
            "vision = \"modalities.input\"\n\
             accepts_temperature = \"capabilities.temperature\"\n\
             reasoning = \"capabilities.reasoning.supported\"\n\
             [pricing]\n\
             input_per_mtok = \"cost.text.input.price\"\n\
             output_per_mtok = \"cost.text.output.price\"\n",
        );
        let found = p
            .read_models(&json!({"data": [
                {"id": "a",
                 "modalities": {"input": ["text", "image/png"]},
                 "capabilities": {"temperature": false, "reasoning": {"supported": true}},
                 "cost": {"text": {"input": {"price": 0.15}, "output": {"price": 0.6}}}},
                {"id": "b",
                 "modalities": {"input": ["text"]},
                 "capabilities": {"reasoning": {"supported": false}},
                 "cost": {"text": {"input": {"price": "0.3"}, "output": {"price": 1.2}}}}
            ]}))
            .unwrap();
        assert_eq!(found["a"]["vision"].as_bool(), Some(true));
        assert_eq!(found["a"]["accepts_temperature"].as_bool(), Some(false));
        assert!(
            found["a"].get("effort").is_none(),
            "a reasoning model keeps its effort levels"
        );
        assert_eq!(
            found["a"]["pricing"]["input_per_mtok"].as_float(),
            Some(0.15)
        );
        assert_eq!(found["b"]["vision"].as_bool(), Some(false));
        assert_eq!(found["b"]["effort"].as_str(), Some("unsupported"));
        assert!(
            found["b"].get("accepts_temperature").is_none(),
            "an unstated fact stays unset"
        );
        assert_eq!(
            found["b"]["pricing"]["output_per_mtok"].as_float(),
            Some(1.2)
        );
        assert!(
            found["b"]["pricing"].get("cache_read_per_mtok").is_none(),
            "an unmapped rate stays out"
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
